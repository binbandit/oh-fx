use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::McpError;
use crate::features::tools::{Tool, ToolCallOutcome, ToolCatalog};
use crate::mcp_contract::McpServerConfig;
use crate::server_connection::{McpClient, ServerNotification};
use crate::server_transport::{ConnectOptions, startup_failure_message};
use crate::timing::spawn;
use crate::tool_operations::CallOptions;
use crate::transport::ShutdownMode;

pub(crate) enum Lifecycle {
    Idle,
    Starting,
    Ready(Arc<McpClient>),
    Failed(String),
}

enum State {
    Waiting,
    Starting,
    Ready(Connection),
    Failed(String),
    Stopped,
}

#[derive(Clone)]
struct Connection {
    client: Arc<McpClient>,
    stop: CancellationToken,
}

pub(crate) enum CallFailure {
    Mcp(McpError),
    RestartFailed(String),
    DefinitionChanged { still_advertised: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Advertised {
    pub(crate) tool: Tool,
    pub(crate) instructions: Option<Arc<str>>,
}

impl From<McpError> for CallFailure {
    fn from(error: McpError) -> Self {
        Self::Mcp(error)
    }
}

pub(crate) struct Server {
    pub(crate) config: McpServerConfig,
    options: ConnectOptions,
    state: Mutex<State>,
    restarts: Mutex<u8>,
    catalog_generation: Arc<AtomicU64>,
}

impl Server {
    pub(crate) fn new(
        config: McpServerConfig,
        options: ConnectOptions,
        catalog_generation: Arc<AtomicU64>,
    ) -> Self {
        Self {
            config,
            options,
            state: Mutex::new(State::Waiting),
            restarts: Mutex::new(0),
            catalog_generation,
        }
    }

    pub(crate) fn lifecycle(&self) -> Lifecycle {
        match &*lock(&self.state) {
            State::Waiting | State::Stopped => Lifecycle::Idle,
            State::Starting => Lifecycle::Starting,
            State::Ready(connection) => Lifecycle::Ready(Arc::clone(&connection.client)),
            State::Failed(message) => Lifecycle::Failed(message.clone()),
        }
    }

    pub(crate) fn restarts(&self) -> u8 {
        *lock(&self.restarts)
    }

    pub(crate) fn catalog(&self) -> Option<(Arc<ToolCatalog>, Option<Arc<str>>)> {
        match &*lock(&self.state) {
            State::Ready(connection) => Some((
                connection.client.tool_catalog(),
                connection
                    .client
                    .server_info()
                    .instructions
                    .as_deref()
                    .map(Arc::from),
            )),
            _ => None,
        }
    }

    pub(crate) async fn start(self: &Arc<Self>) {
        {
            let mut state = lock(&self.state);
            if !matches!(*state, State::Waiting) {
                return;
            }
            *state = State::Starting;
        }
        if let Err(message) = self.connect().await {
            self.settle_failure(message);
        }
    }

    async fn connect(self: &Arc<Self>) -> Result<(), String> {
        let client = McpClient::connect(&self.config, &self.options)
            .await
            .map_err(|failure| startup_failure_message(&failure, self.config.startup_timeout_ms))?;
        let connection = Connection {
            client: Arc::new(client),
            stop: CancellationToken::new(),
        };
        let replaced = {
            let mut state = lock(&self.state);
            if matches!(*state, State::Stopped) {
                None
            } else {
                Some(std::mem::replace(
                    &mut *state,
                    State::Ready(connection.clone()),
                ))
            }
        };
        let Some(replaced) = replaced else {
            connection.client.shutdown(ShutdownMode::Immediate).await;
            return Ok(());
        };
        if let State::Ready(previous) = replaced {
            previous.stop.cancel();
            previous.client.shutdown(ShutdownMode::Immediate).await;
        }
        self.catalog_generation.fetch_add(1, Ordering::AcqRel);
        spawn(watch(Arc::downgrade(self), connection));
        Ok(())
    }

    fn settle_failure(&self, message: String) {
        let mut state = lock(&self.state);
        if !matches!(*state, State::Stopped) {
            *state = State::Failed(message);
        }
        drop(state);
        self.catalog_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) async fn call(
        self: &Arc<Self>,
        advertised: &Advertised,
        arguments: &Value,
        options: CallOptions,
    ) -> Result<ToolCallOutcome, CallFailure> {
        let client = self.running_client().await?;
        let published = client.tool_catalog();
        let catalog = client.current_tools().await?;
        if !Arc::ptr_eq(&published, &catalog) {
            self.catalog_generation.fetch_add(1, Ordering::AcqRel);
        }
        let name = &advertised.tool.name;
        let current = catalog.get(name);
        let instructions = client.server_info().instructions.as_deref();
        if current != Some(&advertised.tool) || instructions != advertised.instructions.as_deref() {
            self.catalog_generation.fetch_add(1, Ordering::AcqRel);
            return Err(CallFailure::DefinitionChanged {
                still_advertised: current.is_some(),
            });
        }
        Ok(client.call_tool(name, arguments, options).await?)
    }

    async fn running_client(self: &Arc<Self>) -> Result<Arc<McpClient>, CallFailure> {
        let current = match &*lock(&self.state) {
            State::Ready(connection) => Some(connection.client.clone()),
            _ => None,
        };
        let client = current.ok_or(CallFailure::Mcp(McpError::McpConnectionClosed))?;
        if client.is_running() {
            return Ok(client);
        }
        {
            let mut restarts = lock(&self.restarts);
            if *restarts >= self.config.restart_limit {
                drop(restarts);
                self.settle_failure("MCP restart limit reached".to_owned());
                return Err(CallFailure::Mcp(McpError::McpRestartLimitReached));
            }
            *restarts += 1;
        }
        self.connect().await.map_err(|message| {
            self.settle_failure(message.clone());
            CallFailure::RestartFailed(message)
        })?;
        match &*lock(&self.state) {
            State::Ready(connection) => Ok(connection.client.clone()),
            _ => Err(CallFailure::Mcp(McpError::McpConnectionClosed)),
        }
    }

    pub(crate) async fn stop(&self, mode: ShutdownMode) {
        if let Some(client) = self.retire() {
            client.shutdown(mode).await;
        }
    }

    pub(crate) fn retire(&self) -> Option<Arc<McpClient>> {
        let previous = std::mem::replace(&mut *lock(&self.state), State::Stopped);
        let State::Ready(connection) = previous else {
            return None;
        };
        connection.stop.cancel();
        Some(connection.client)
    }
}

async fn watch(server: Weak<Server>, connection: Connection) {
    loop {
        let Some(notification) = connection
            .stop
            .run_until_cancelled(connection.client.next_notification())
            .await
        else {
            return;
        };
        match notification {
            Some(ServerNotification::ToolsListChanged) => {
                let Some(refreshed) = connection
                    .stop
                    .run_until_cancelled(connection.client.list_tools())
                    .await
                else {
                    return;
                };
                let Some(server) = server.upgrade() else {
                    return;
                };
                if refreshed.is_ok() {
                    server.catalog_generation.fetch_add(1, Ordering::AcqRel);
                }
            }
            Some(_) => {}
            None => return,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
