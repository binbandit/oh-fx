use std::env;
use std::fmt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use ofx_http::ConnectionOptions;
use ofx_text::{HeadRounding, encode_terminal_safe, mask_secrets, write_head_tail_bounded};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::auth_state::AuthState;
use crate::error::McpError;
use crate::features::tools::{CatalogBuilder, Limits, Listing};
use crate::legacy_http_sse::{LegacySseClient, SSE_PROTOCOL_VERSION, validate_initialize_response};
use crate::legacy_streamable_http::{
    HTTP_INITIALIZED_NOTIFICATION, HttpEndpoint, HttpVersion, LegacyHttpClient,
};
use crate::mcp_auth_store::CredentialStore;
use crate::mcp_contract::McpServerConfig;
use crate::operation_control::monotonic_millis;
use crate::protocol_messages::{
    ElicitationCapabilities, STDIO_INITIALIZED_NOTIFICATION, ServerCapabilities,
    build_legacy_initialize_request, build_tools_list_request, parse_json,
    parse_server_capabilities, parse_server_identity, parse_server_instructions,
};
use crate::protocol_negotiation::{
    ElicitationWire, LegacyInitializeObservation, LegacyInitializeTransition, LegacyStdioVersion,
    PROTOCOL_VERSION_ENVIRONMENT, classify_legacy_initialize_response,
    decide_legacy_initialize_transition, validate_startup_mode,
};
use crate::server_auth::HttpAuth;
use crate::stdio_dispatcher::{
    ChildDiagnostics, StderrCapture, StdioDispatcher, StdioLaunch, StopMode,
};
use crate::streamable_http::validate_endpoint;
use crate::timing::timeout_at;
use crate::transport::{McpTransport, ShutdownMode, Transport, TransportRequest};

pub(crate) const DISCOVERY_RESPONSE_FRAME_CAP_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectOptions {
    pub client_version: String,
    pub user_agent: String,
    pub profile_data: Option<PathBuf>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        let client_version = env!("CARGO_PKG_VERSION").to_owned();
        Self {
            user_agent: format!("oh-fx/{client_version}"),
            client_version,
            profile_data: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerInfo {
    pub protocol_version: &'static str,
    pub name: Option<String>,
    pub version: Option<String>,
    pub instructions: Option<String>,
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StartupFailure {
    pub error: McpError,
    pub diagnostics: Option<ChildDiagnostics>,
    pub message: Option<String>,
}

impl StartupFailure {
    fn new(error: McpError, diagnostics: Option<ChildDiagnostics>) -> Self {
        Self {
            error,
            diagnostics,
            message: None,
        }
    }

    pub(crate) fn explained(error: McpError, message: Option<String>) -> Self {
        Self {
            message,
            ..Self::new(error, None)
        }
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
    pub(crate) transport: Transport,
    pub(crate) info: ServerInfo,
    pub(crate) wire: Option<ElicitationWire>,
    pub(crate) listing: Listing,
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
    deadline: Instant,
) -> Result<Connected, StartupFailure> {
    let launch = stdio_launch(config)?;
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
    let started = Started {
        info: server_info(&handshake.response, handshake.version.as_str()),
        transport: Transport::Stdio(handshake.dispatcher),
        wire: handshake.version.wire(),
        notifications: handshake.notifications,
    };
    finish_startup(
        started,
        STDIO_INITIALIZED_NOTIFICATION,
        deadline,
        stdio_discovery_error,
    )
    .await
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
        let observation = parse_json(response.as_bytes())
            .ok_or(McpError::McpInvalidJson)
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

pub(crate) async fn connect_http(
    config: &McpServerConfig,
    options: &ConnectOptions,
    deadline: Instant,
    state: &Arc<AuthState>,
) -> Result<Connected, StartupFailure> {
    validate_startup_mode(
        &config.env,
        env::var(PROTOCOL_VERSION_ENVIRONMENT).ok().as_deref(),
    )?;
    let (endpoint, notifications) = http_endpoint(config, options, state).await?;
    let auth = Arc::clone(&endpoint.auth);
    let preferred = HttpVersion::PREFERRED;
    let body = build_legacy_initialize_request(
        1,
        preferred.as_str(),
        preferred.wire(),
        ElicitationCapabilities::default(),
        &options.client_version,
    );
    let initialize =
        LegacyHttpClient::initialize(endpoint, &body, 1, DISCOVERY_RESPONSE_FRAME_CAP_BYTES);
    let (client, response) = timeout_at(deadline, initialize)
        .await
        .map_err(|_| McpError::McpRequestTimedOut)?
        .map_err(|error| auth.startup_failure(error.into()))?;
    let started = Started {
        info: server_info(&response, client.version().as_str()),
        wire: client.version().wire(),
        transport: Transport::Http(client),
        notifications,
    };
    let connected = finish_startup(started, HTTP_INITIALIZED_NOTIFICATION, deadline, same_error)
        .await
        .map_err(|failure| auth.startup_failure(failure))?;
    if let Transport::Http(client) = &connected.transport
        && wants_notification_stream(connected.info.capabilities)
    {
        client.start_notification_listener();
    }
    Ok(connected)
}

async fn http_endpoint(
    config: &McpServerConfig,
    options: &ConnectOptions,
    state: &Arc<AuthState>,
) -> Result<(HttpEndpoint, mpsc::UnboundedReceiver<Value>), StartupFailure> {
    let url = config.remote_url().map_err(McpError::from)?;
    validate_endpoint(url).map_err(McpError::from)?;
    let client = |follow_redirects| {
        ofx_http::build_connection_client(&ConnectionOptions {
            user_agent: options.user_agent.clone(),
            follow_redirects,
            ..ConnectionOptions::default()
        })
        .map_err(|_| McpError::HttpClientUnavailable)
    };
    let store = options.profile_data.as_deref().map(CredentialStore::new);
    let auth = HttpAuth::resolve(config, store, state, || client(false), &|name| {
        env::var(name).ok()
    })
    .await?;
    let (sender, notifications) = mpsc::unbounded_channel();
    let endpoint = HttpEndpoint {
        http: client(ConnectionOptions::default().follow_redirects)?,
        url: url.to_owned(),
        auth: Arc::new(auth),
        notifications: sender,
    };
    Ok((endpoint, notifications))
}

pub(crate) async fn connect_sse(
    config: &McpServerConfig,
    options: &ConnectOptions,
    deadline: Instant,
    state: &Arc<AuthState>,
) -> Result<Connected, StartupFailure> {
    let (endpoint, notifications) = http_endpoint(config, options, state).await?;
    let auth = Arc::clone(&endpoint.auth);
    let client = LegacySseClient::connect(endpoint, DISCOVERY_RESPONSE_FRAME_CAP_BYTES, deadline)
        .await
        .map_err(|error| auth.startup_failure(error.into()))?;
    let transport = Transport::Sse(client);
    let started = Started {
        info: initialize_sse(&transport, options, deadline).await,
        transport,
        wire: None,
        notifications,
    };
    finish_startup(started, HTTP_INITIALIZED_NOTIFICATION, deadline, same_error)
        .await
        .map_err(|failure| auth.startup_failure(failure))
}

async fn initialize_sse(
    transport: &Transport,
    options: &ConnectOptions,
    deadline: Instant,
) -> Result<ServerInfo, McpError> {
    let id = transport.next_request_id()?;
    let body = build_legacy_initialize_request(
        id,
        SSE_PROTOCOL_VERSION,
        None,
        ElicitationCapabilities::default(),
        &options.client_version,
    );
    let response = transport
        .request(TransportRequest::new(
            id,
            body,
            DISCOVERY_RESPONSE_FRAME_CAP_BYTES,
            deadline,
        ))
        .await?;
    let value = parse_json(response.as_bytes()).ok_or(McpError::McpInvalidJson)?;
    validate_initialize_response(&value)?;
    server_info(&value, SSE_PROTOCOL_VERSION)
}

struct Started {
    transport: Transport,
    info: Result<ServerInfo, McpError>,
    wire: Option<ElicitationWire>,
    notifications: mpsc::UnboundedReceiver<Value>,
}

async fn finish_startup(
    started: Started,
    initialized: &str,
    deadline: Instant,
    request_error: fn(McpError) -> McpError,
) -> Result<Connected, StartupFailure> {
    let Started {
        transport,
        info,
        wire,
        notifications,
    } = started;
    match discover(&transport, info, initialized, deadline, request_error).await {
        Ok((info, listing)) => Ok(Connected {
            transport,
            info,
            wire,
            listing,
            notifications,
        }),
        Err(error) => Err(abandon(transport, error).await),
    }
}

async fn discover(
    transport: &Transport,
    info: Result<ServerInfo, McpError>,
    initialized: &str,
    deadline: Instant,
    request_error: fn(McpError) -> McpError,
) -> Result<(ServerInfo, Listing), McpError> {
    let info = info?;
    transport.notify(initialized.to_owned(), deadline).await?;
    let listing = discover_tools(transport, deadline, request_error).await?;
    Ok((info, listing))
}

async fn abandon(transport: Transport, error: McpError) -> StartupFailure {
    match transport {
        Transport::Stdio(dispatcher) => fail_launch(dispatcher, error, StopMode::Graceful).await,
        transport => {
            transport.shutdown(ShutdownMode::Graceful).await;
            error.into()
        }
    }
}

fn wants_notification_stream(capabilities: ServerCapabilities) -> bool {
    capabilities.tools_list_changed
        || capabilities
            .resources
            .is_some_and(|resources| resources.list_changed)
        || capabilities
            .prompts
            .is_some_and(|prompts| prompts.list_changed)
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

fn same_error(error: McpError) -> McpError {
    error
}

fn stdio_discovery_error(error: McpError) -> McpError {
    match error {
        McpError::Cancelled | McpError::McpRequestTimedOut => error,
        _ => McpError::McpToolDiscoveryFailed,
    }
}

pub(crate) async fn discover_tools(
    transport: &Transport,
    deadline: Instant,
    request_error: fn(McpError) -> McpError,
) -> Result<Listing, McpError> {
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
        if builder.append_response(&response, monotonic_millis(), Limits::default())? {
            return builder.finish();
        }
    }
}

const STDERR_DISPLAY_BYTES: usize = 400;
const WORD_SEPARATORS: [char; 4] = [' ', '\t', '\r', '\n'];

pub(crate) fn startup_failure_message(
    failure: &StartupFailure,
    span_ms: u32,
    startup_timeout_ms: u32,
) -> String {
    if let Some(message) = &failure.message {
        return message.clone();
    }
    let diagnostics = failure.diagnostics.as_ref();
    if let Some(diagnostics) = diagnostics
        && let Some(rejected) = &diagnostics.rejected_output
    {
        let mut message = String::from(
            "MCP server wrote output that is not an MCP message before completing startup",
        );
        let mut line = without_ansi(&rejected.bytes);
        if rejected.truncated {
            line = without_trailing_word(&line).to_owned();
        }
        let text = display_plain(&line);
        if !text.is_empty() {
            message.push_str(": ");
            message.push_str(&text);
        }
        let stderr = display_stderr(&diagnostics.stderr);
        if !stderr.is_empty() {
            message.push_str("; stderr: ");
            message.push_str(&stderr);
        }
        return message;
    }
    match failure.error {
        McpError::McpServerExitedDuringStartup | McpError::McpConnectionClosed => {
            let mut message = format!(
                "MCP server {} before completing startup",
                term_phrase(diagnostics.and_then(|diagnostics| diagnostics.status))
            );
            push_stderr_suffix(&mut message, diagnostics);
            message
        }
        McpError::McpRequestTimedOut => {
            let mut message = format!("MCP server did not complete startup within {span_ms} ms");
            if span_ms == startup_timeout_ms {
                message.push_str(" (startup_timeout_ms)");
            }
            match diagnostics {
                Some(earlier) if earlier.status.is_some() => {
                    message.push_str("; an earlier launch ");
                    message.push_str(&term_phrase(earlier.status));
                    push_stderr_suffix(&mut message, Some(earlier));
                }
                Some(live) => {
                    let stderr = display_stderr(&live.stderr);
                    if !stderr.is_empty() {
                        message.push_str("; last stderr: ");
                        message.push_str(&stderr);
                    }
                }
                None => {}
            }
            message
        }
        ref error => error.to_string(),
    }
}

fn push_stderr_suffix(message: &mut String, diagnostics: Option<&ChildDiagnostics>) {
    let Some(diagnostics) = diagnostics else {
        return;
    };
    let stderr = display_stderr(&diagnostics.stderr);
    if !stderr.is_empty() {
        message.push_str(": ");
        message.push_str(&stderr);
    }
}

fn term_phrase(status: Option<ExitStatus>) -> String {
    let Some(status) = status else {
        return "closed its connection".to_owned();
    };
    if let Some(code) = status.code() {
        format!("exited with code {code}")
    } else if let Some(signal) = status.signal() {
        format!("was killed by signal {signal}")
    } else if let Some(signal) = status.stopped_signal() {
        format!("was stopped by signal {signal}")
    } else {
        format!("ended with status {}", status.into_raw())
    }
}

fn display_stderr(capture: &StderrCapture) -> String {
    if !capture.omitted() {
        let mut joined = capture.head().to_vec();
        joined.extend_from_slice(capture.tail());
        return display_plain(&without_ansi(&joined));
    }
    let head = without_ansi(capture.head());
    let tail = without_ansi(capture.tail());
    let joined = format!(
        "{} ... {}",
        without_trailing_word(&head),
        without_leading_word(&tail)
    );
    display_plain(&joined)
}

fn display_plain(plain: &str) -> String {
    let masked = mask_secrets(plain);
    let mut flattened = String::with_capacity(masked.len());
    for word in masked.split_ascii_whitespace() {
        if !flattened.is_empty() {
            flattened.push(' ');
        }
        flattened.push_str(word);
    }
    let encoded = encode_terminal_safe(flattened.as_bytes(), usize::MAX).text;
    String::from_utf8_lossy(&write_head_tail_bounded(
        encoded.as_bytes(),
        STDERR_DISPLAY_BYTES,
        " ... ",
        HeadRounding::Down,
    ))
    .into_owned()
}

fn without_trailing_word(text: &str) -> &str {
    text.rfind(WORD_SEPARATORS)
        .and_then(|end| text.get(..=end))
        .unwrap_or_default()
}

fn without_leading_word(text: &str) -> &str {
    text.find(WORD_SEPARATORS)
        .and_then(|start| text.get(start..))
        .unwrap_or_default()
}

fn without_ansi(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut rest = raw;
    while let Some((text, sequence)) = rest
        .iter()
        .position(|byte| *byte == 0x1b)
        .and_then(|escape| rest.split_at_checked(escape))
    {
        out.extend_from_slice(text);
        rest = sequence
            .get(ansi_sequence_len(sequence)..)
            .unwrap_or_default();
    }
    out.extend_from_slice(rest);
    String::from_utf8_lossy(&out).into_owned()
}

fn ansi_sequence_len(sequence: &[u8]) -> usize {
    match sequence.get(1) {
        None => sequence.len(),
        Some(b'[') => sequence
            .iter()
            .skip(2)
            .position(|byte| (b'@'..=b'~').contains(byte))
            .map_or(sequence.len(), |offset| offset + 3),
        Some(b']') => {
            let mut position = 2;
            while let Some(byte) = sequence.get(position) {
                if *byte == 0x07 {
                    return position + 1;
                }
                if *byte == 0x1b && sequence.get(position + 1) == Some(&b'\\') {
                    return position + 2;
                }
                position += 1;
            }
            sequence.len()
        }
        Some(_) => 2,
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
