use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::catalog_freshness::Freshness;
use crate::error::McpError;
use crate::feature_catalog::{FeatureCatalog, FeatureCatalogs, Snapshot};
use crate::features::tools::{Tool, ToolCallOutcome, ToolCatalog};
use crate::mcp_contract::McpServerConfig;
use crate::server_connection::{McpClient, ServerNotification};
use crate::server_transport::{ConnectOptions, StartupFailure, startup_failure_message};
use crate::timing::{spawn, timeout_at};
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
    DefinitionChanged,
}

pub(crate) enum RestartFailure {
    Unavailable(McpError),
    Failed { error: McpError, message: String },
}

impl RestartFailure {
    pub(crate) fn into_error(self) -> McpError {
        match self {
            Self::Unavailable(error) | Self::Failed { error, .. } => error,
        }
    }
}

impl From<RestartFailure> for CallFailure {
    fn from(failure: RestartFailure) -> Self {
        match failure {
            RestartFailure::Unavailable(error) => Self::Mcp(error),
            RestartFailure::Failed { message, .. } => Self::RestartFailed(message),
        }
    }
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
    pub(crate) features: FeatureCatalogs,
    options: ConnectOptions,
    state: Mutex<State>,
    recovery: tokio::sync::Mutex<()>,
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
            features: FeatureCatalogs::default(),
            options,
            state: Mutex::new(State::Waiting),
            recovery: tokio::sync::Mutex::new(()),
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
        if let Err(failure) = self
            .connect(McpClient::connect(&self.config, &self.options))
            .await
        {
            let timeout_ms = self.config.startup_timeout_ms;
            self.settle_failure(startup_failure_message(&failure, timeout_ms, timeout_ms));
        }
    }

    async fn connect(
        self: &Arc<Self>,
        connecting: impl Future<Output = Result<McpClient, StartupFailure>>,
    ) -> Result<(), StartupFailure> {
        let client = connecting.await?;
        let connection = Connection {
            client: Arc::new(client),
            stop: CancellationToken::new(),
        };
        let replaced = {
            let mut state = lock(&self.state);
            if matches!(*state, State::Stopped) {
                None
            } else {
                self.features
                    .reset(connection.client.server_info().capabilities);
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
        arguments_json: &str,
        options: CallOptions,
    ) -> Result<ToolCallOutcome, CallFailure> {
        let deadline =
            Instant::now() + Duration::from_millis(self.config.operation_timeout_ms.into());
        let client = self.running_client(deadline).await?;
        let refreshed = client.settled_tools(deadline).await?;
        if refreshed.replaced {
            self.catalog_generation.fetch_add(1, Ordering::AcqRel);
        }
        if client.tools_invalidation.pending() {
            return Err(McpError::McpToolCatalogChanged.into());
        }
        let catalog = refreshed.catalog;
        let name = &advertised.tool.name;
        let current = catalog.get(name);
        let instructions = client.server_info().instructions.as_deref();
        if current != Some(&advertised.tool) || instructions != advertised.instructions.as_deref() {
            self.catalog_generation.fetch_add(1, Ordering::AcqRel);
            return Err(CallFailure::DefinitionChanged);
        }
        if Instant::now() >= deadline {
            return Err(McpError::McpRequestTimedOut.into());
        }
        Ok(client
            .call_tool(name, arguments_json, options, deadline)
            .await?)
    }

    pub(crate) async fn refresh_tools(&self, client: &McpClient, deadline: Instant) {
        if client.refresh_tools(deadline).await.replaced {
            self.catalog_generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    async fn follow_tool_change(&self, client: &McpClient, deadline: Instant) {
        while let Ok(refreshed) = client.settled_tools(deadline).await {
            if refreshed.replaced {
                self.catalog_generation.fetch_add(1, Ordering::AcqRel);
            }
            if !client.tools_invalidation.pending()
                || client.tool_snapshot().metadata.freshness == Freshness::FailedRefresh
            {
                return;
            }
        }
    }

    pub(crate) async fn running_client(
        self: &Arc<Self>,
        deadline: Instant,
    ) -> Result<Arc<McpClient>, RestartFailure> {
        let client = self
            .ready_client()
            .ok_or(RestartFailure::Unavailable(McpError::McpConnectionClosed))?;
        if client.is_running() {
            return Ok(client);
        }
        let _recovering = timeout_at(deadline, self.recovery.lock())
            .await
            .map_err(|_| RestartFailure::Unavailable(McpError::McpRequestTimedOut))?;
        let client = self
            .ready_client()
            .ok_or(RestartFailure::Unavailable(McpError::McpConnectionClosed))?;
        if client.is_running() {
            return Ok(client);
        }
        {
            let mut restarts = lock(&self.restarts);
            if *restarts >= self.config.restart_limit {
                drop(restarts);
                self.settle_failure("MCP restart limit reached".to_owned());
                return Err(RestartFailure::Unavailable(
                    McpError::McpRestartLimitReached,
                ));
            }
            *restarts += 1;
        }
        let span_ms = millis_until(deadline);
        if span_ms == 0 {
            return Err(RestartFailure::Unavailable(McpError::McpRequestTimedOut));
        }
        if let Err(failure) = self
            .connect(Box::pin(McpClient::connect_until(
                &self.config,
                &self.options,
                deadline,
            )))
            .await
        {
            let message =
                startup_failure_message(&failure, span_ms, self.config.startup_timeout_ms);
            self.settle_failure(message.clone());
            return Err(RestartFailure::Failed {
                error: failure.error,
                message,
            });
        }
        self.ready_client()
            .ok_or(RestartFailure::Unavailable(McpError::McpConnectionClosed))
    }

    pub(crate) fn publish_catalog<T: FeatureCatalog>(
        &self,
        client: &Arc<McpClient>,
        snapshot: Snapshot<T>,
    ) -> bool {
        self.while_current(client, || self.features.publish(snapshot))
            .is_some()
    }

    pub(crate) fn while_current<R>(
        &self,
        client: &Arc<McpClient>,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        let state = lock(&self.state);
        matches!(&*state, State::Ready(connection) if Arc::ptr_eq(&connection.client, client))
            .then(action)
    }

    fn ready_client(&self) -> Option<Arc<McpClient>> {
        match &*lock(&self.state) {
            State::Ready(connection) => Some(connection.client.clone()),
            _ => None,
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
                let Some(current) = server.upgrade() else {
                    return;
                };
                let deadline = Instant::now() + connection.client.operation_timeout;
                if connection
                    .stop
                    .run_until_cancelled(current.follow_tool_change(&connection.client, deadline))
                    .await
                    .is_none()
                {
                    return;
                }
            }
            Some(_) => {}
            None => return,
        }
    }
}

fn millis_until(deadline: Instant) -> u32 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    u32::try_from(remaining.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
