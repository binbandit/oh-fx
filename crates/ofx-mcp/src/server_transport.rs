use std::env;
use std::fmt;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::error::McpError;
use crate::features::tools::{CatalogBuilder, Limits, ToolCatalog};
use crate::mcp_contract::McpServerConfig;
use crate::protocol_messages::{
    ElicitationCapabilities, STDIO_INITIALIZED_NOTIFICATION, ServerCapabilities,
    build_legacy_initialize_request, build_tools_list_request, parse_server_capabilities,
    parse_server_identity, parse_server_instructions,
};
use crate::protocol_negotiation::{
    ElicitationWire, LegacyInitializeObservation, LegacyInitializeTransition, LegacyStdioVersion,
    PROTOCOL_VERSION_ENVIRONMENT, classify_legacy_initialize_response,
    decide_legacy_initialize_transition, validate_startup_mode,
};
use crate::stdio_dispatcher::{ChildDiagnostics, StdioDispatcher, StdioLaunch, StopMode};
use crate::transport::{McpTransport, TransportRequest};

pub(crate) const DISCOVERY_RESPONSE_FRAME_CAP_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectOptions {
    pub client_version: String,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub protocol_version: &'static str,
    pub name: Option<String>,
    pub version: Option<String>,
    pub instructions: Option<String>,
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StartupFailure {
    pub error: McpError,
    pub diagnostics: Option<ChildDiagnostics>,
}

impl StartupFailure {
    fn new(error: McpError, diagnostics: Option<ChildDiagnostics>) -> Self {
        Self { error, diagnostics }
    }
}

impl From<McpError> for StartupFailure {
    fn from(error: McpError) -> Self {
        Self::new(error, None)
    }
}

impl fmt::Display for StartupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for StartupFailure {}

pub(crate) struct Connected {
    pub(crate) transport: Box<dyn McpTransport>,
    pub(crate) info: ServerInfo,
    pub(crate) wire: Option<ElicitationWire>,
    pub(crate) catalog: ToolCatalog,
    pub(crate) notifications: mpsc::UnboundedReceiver<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupRestartDecision {
    Restart,
    StopLimit,
    StopDeadlineSpent,
    StopChildExited,
}

fn decide_startup_restart(
    attempts: u8,
    limit: u8,
    deadline_spent: bool,
    child_exited: bool,
) -> StartupRestartDecision {
    if attempts >= limit {
        StartupRestartDecision::StopLimit
    } else if deadline_spent {
        StartupRestartDecision::StopDeadlineSpent
    } else if child_exited {
        StartupRestartDecision::StopChildExited
    } else {
        StartupRestartDecision::Restart
    }
}

pub(crate) fn startup_deadline(config: &McpServerConfig) -> Instant {
    Instant::now() + Duration::from_millis(config.startup_timeout_ms.into())
}

pub(crate) async fn connect_stdio(
    config: &McpServerConfig,
    options: &ConnectOptions,
) -> Result<Connected, StartupFailure> {
    let launch = stdio_launch(config)?;
    let deadline = startup_deadline(config);
    let mut restart_attempts = 0_u8;
    loop {
        let failure = match connect_stdio_once(&launch, options, deadline).await {
            Ok(connected) => return Ok(connected),
            Err(failure) => failure,
        };
        if failure.error == McpError::Cancelled {
            return Err(failure);
        }
        let decision = decide_startup_restart(
            restart_attempts,
            config.restart_limit,
            Instant::now() >= deadline,
            failure.error == McpError::McpServerExitedDuringStartup,
        );
        if decision != StartupRestartDecision::Restart {
            return Err(failure);
        }
        restart_attempts += 1;
    }
}

fn stdio_launch(config: &McpServerConfig) -> Result<StdioLaunch, McpError> {
    let command = config.stdio_command()?;
    validate_startup_mode(
        &config.env,
        env::var(PROTOCOL_VERSION_ENVIRONMENT).ok().as_deref(),
    )?;
    let mut argv = Vec::with_capacity(config.args.len() + 1);
    argv.push(command.to_owned());
    argv.extend(config.args.iter().cloned());
    let environment = config
        .env
        .iter()
        .filter(|entry| entry.key != PROTOCOL_VERSION_ENVIRONMENT)
        .map(|entry| (entry.key.clone(), entry.value.clone()))
        .collect();
    Ok(StdioLaunch {
        argv,
        environment,
        cwd: config.cwd.clone(),
        initial_max_frame_bytes: DISCOVERY_RESPONSE_FRAME_CAP_BYTES,
    })
}

struct Handshake {
    dispatcher: StdioDispatcher,
    notifications: mpsc::UnboundedReceiver<Value>,
    response: Value,
    version: LegacyStdioVersion,
}

async fn connect_stdio_once(
    launch: &StdioLaunch,
    options: &ConnectOptions,
    deadline: Instant,
) -> Result<Connected, StartupFailure> {
    let handshake = negotiate_stdio(launch, options, deadline).await?;
    let dispatcher = handshake.dispatcher;
    match finish_stdio_startup(
        &dispatcher,
        &handshake.response,
        handshake.version,
        deadline,
    )
    .await
    {
        Ok((info, catalog)) => Ok(Connected {
            transport: Box::new(dispatcher),
            info,
            wire: handshake.version.wire(),
            catalog,
            notifications: handshake.notifications,
        }),
        Err(error) => Err(fail_launch(dispatcher, error, StopMode::Graceful).await),
    }
}

async fn negotiate_stdio(
    launch: &StdioLaunch,
    options: &ConnectOptions,
    deadline: Instant,
) -> Result<Handshake, StartupFailure> {
    let mut offered = LegacyStdioVersion::V2025_11_25;
    let mut last_exit = None;
    loop {
        if Instant::now() >= deadline {
            return Err(StartupFailure::new(McpError::McpRequestTimedOut, last_exit));
        }
        let (sender, notifications) = mpsc::unbounded_channel();
        let dispatcher = StdioDispatcher::spawn(launch.clone(), sender)?;
        let response = match initialize_once(&dispatcher, offered, options, deadline).await {
            Ok(response) => response,
            Err(McpError::McpConnectionClosed) => {
                last_exit = Some(dispatcher.settled_diagnostics().await);
                dispatcher.stop(StopMode::Forced).await;
                match decide_legacy_initialize_transition(
                    offered,
                    LegacyInitializeObservation::ConnectionClosed,
                ) {
                    LegacyInitializeTransition::Retry(next) => {
                        offered = next;
                        continue;
                    }
                    _ => {
                        return Err(StartupFailure::new(
                            McpError::McpServerExitedDuringStartup,
                            last_exit,
                        ));
                    }
                }
            }
            Err(error @ (McpError::McpRequestTimedOut | McpError::Cancelled)) => {
                return Err(fail_launch(dispatcher, error, StopMode::Graceful).await);
            }
            Err(_) => {
                return Err(
                    fail_launch(dispatcher, McpError::McpInitFailed, StopMode::Graceful).await,
                );
            }
        };
        let observation = serde_json::from_str::<Value>(&response)
            .map_err(|_| McpError::McpInvalidJson)
            .and_then(|value| {
                classify_legacy_initialize_response(&value, offered)
                    .map(|observation| (value, observation))
            });
        let (value, observation) = match observation {
            Ok(classified) => classified,
            Err(error) => return Err(fail_launch(dispatcher, error, StopMode::Graceful).await),
        };
        match decide_legacy_initialize_transition(offered, observation) {
            LegacyInitializeTransition::Accept(version) => {
                return Ok(Handshake {
                    dispatcher,
                    notifications,
                    response: value,
                    version,
                });
            }
            LegacyInitializeTransition::Retry(next) => {
                dispatcher.stop(StopMode::Forced).await;
                offered = next;
            }
            LegacyInitializeTransition::Fail => {
                return Err(fail_launch(
                    dispatcher,
                    McpError::McpUnsupportedProtocolVersion,
                    StopMode::Graceful,
                )
                .await);
            }
        }
    }
}

async fn initialize_once(
    dispatcher: &StdioDispatcher,
    offered: LegacyStdioVersion,
    options: &ConnectOptions,
    deadline: Instant,
) -> Result<String, McpError> {
    let id = dispatcher.next_request_id()?;
    let body = build_legacy_initialize_request(
        id,
        offered.as_str(),
        offered.wire(),
        ElicitationCapabilities::default(),
        &options.client_version,
    );
    dispatcher
        .request(TransportRequest::new(
            id,
            body,
            DISCOVERY_RESPONSE_FRAME_CAP_BYTES,
            deadline,
        ))
        .await
}

async fn fail_launch(
    dispatcher: StdioDispatcher,
    error: McpError,
    mode: StopMode,
) -> StartupFailure {
    let diagnostics = if error == McpError::McpRequestTimedOut {
        dispatcher.child_diagnostics()
    } else {
        dispatcher.settled_diagnostics().await
    };
    dispatcher.stop(mode).await;
    StartupFailure::new(error, Some(diagnostics))
}

async fn finish_stdio_startup(
    dispatcher: &StdioDispatcher,
    response: &Value,
    version: LegacyStdioVersion,
    deadline: Instant,
) -> Result<(ServerInfo, ToolCatalog), McpError> {
    let info = server_info(response, version.as_str())?;
    dispatcher
        .notify(STDIO_INITIALIZED_NOTIFICATION.to_owned(), deadline)
        .await?;
    let catalog = discover_tools(dispatcher, deadline, stdio_discovery_error).await?;
    Ok((info, catalog))
}

pub(crate) fn server_info(
    response: &Value,
    protocol_version: &'static str,
) -> Result<ServerInfo, McpError> {
    let capabilities = parse_server_capabilities(response)?;
    let identity = parse_server_identity(response)?;
    let instructions = parse_server_instructions(response)?;
    Ok(ServerInfo {
        protocol_version,
        name: identity.name,
        version: identity.version,
        instructions,
        capabilities,
    })
}

fn stdio_discovery_error(error: McpError) -> McpError {
    match error {
        McpError::Cancelled | McpError::McpRequestTimedOut => error,
        _ => McpError::McpToolDiscoveryFailed,
    }
}

pub(crate) async fn discover_tools(
    transport: &dyn McpTransport,
    deadline: Instant,
    request_error: fn(McpError) -> McpError,
) -> Result<ToolCatalog, McpError> {
    let mut builder = CatalogBuilder::default();
    loop {
        let id = transport.next_request_id()?;
        let body = build_tools_list_request(id, builder.next_cursor());
        let response = transport
            .request(TransportRequest::new(
                id,
                body,
                DISCOVERY_RESPONSE_FRAME_CAP_BYTES,
                deadline,
            ))
            .await
            .map_err(request_error)?;
        if builder.append_response(&response, Limits::default())? {
            return builder.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_restart_runs_only_with_budget_left_and_a_failure_a_relaunch_could_change() {
        for deadline_spent in [false, true] {
            for child_exited in [false, true] {
                for attempts in 0..3 {
                    for limit in 0..3 {
                        let decision =
                            decide_startup_restart(attempts, limit, deadline_spent, child_exited);
                        let expect_restart = attempts < limit && !deadline_spent && !child_exited;
                        assert_eq!(decision == StartupRestartDecision::Restart, expect_restart);
                    }
                }
            }
        }
    }

    #[test]
    fn stdio_launch_keeps_the_protocol_variable_away_from_the_child() {
        let mut config = McpServerConfig::stdio("fixture", "node", vec!["server.js".to_owned()]);
        config.env = vec![
            crate::mcp_contract::EnvVar {
                key: PROTOCOL_VERSION_ENVIRONMENT.to_owned(),
                value: "2025-11-25".to_owned(),
            },
            crate::mcp_contract::EnvVar {
                key: "TOKEN".to_owned(),
                value: "value".to_owned(),
            },
        ];
        let launch = stdio_launch(&config).unwrap();
        assert_eq!(launch.argv, ["node", "server.js"]);
        assert_eq!(
            launch.environment,
            [("TOKEN".to_owned(), "value".to_owned())]
        );
        config.command = Some(String::new());
        assert_eq!(stdio_launch(&config), Err(McpError::McpInvalidServerConfig));
    }
}
