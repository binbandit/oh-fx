use std::sync::atomic::Ordering;

use ofx_text::encode_terminal_safe;

use crate::health::{
    AuthenticationState, CacheFreshness, CapabilityCounts, ConnectionState, ServerSnapshot, Status,
    SubscriptionState, capability_count, classify, observed_connection,
};
use crate::mcp_contract::{McpServerConfig, TransportType};
use crate::server_connection::McpClient;
use crate::server_lifecycle::{Lifecycle, Server};
use crate::startup_admission::{StartupDecision, StartupPhase, decide_startup};

const NAME_BYTES: usize = 256;
const VERSION_BYTES: usize = 128;
const DISABLED_REQUIRED: &str = "Enable this required server or mark it optional.";
const FAILED_WITHOUT_DETAIL: &str =
    "Connection or discovery failed; check the trusted profile configuration and trace logs.";

pub(crate) fn snapshot_server(server: &Server) -> ServerSnapshot {
    let config = &server.config;
    let (connection, client, last_error) = match server.lifecycle() {
        Lifecycle::Idle
            if decide_startup(config, StartupPhase::All) == StartupDecision::Disabled =>
        {
            (ConnectionState::Disabled, None, None)
        }
        Lifecycle::Idle => (ConnectionState::Disconnected, None, None),
        Lifecycle::Starting => (ConnectionState::Connecting, None, None),
        Lifecycle::Failed(message) => (ConnectionState::Failed, None, Some(message)),
        Lifecycle::Ready(client) => {
            let running = (config.transport == TransportType::Stdio).then(|| client.is_running());
            (
                observed_connection(ConnectionState::Ready, running),
                Some(client),
                None,
            )
        }
    };
    let failure = health_failure(config.required, connection, last_error);
    let mut snapshot = ServerSnapshot {
        configured_name: safe(&config.name, NAME_BYTES),
        source: config.source,
        scope: config.scope,
        workspace_admission: config.workspace_admission,
        required: config.required,
        transport: config.transport,
        negotiated_name: None,
        negotiated_version: None,
        protocol_version: None,
        connection,
        authentication: authentication(config),
        counts: CapabilityCounts::default(),
        cache_freshness: CacheFreshness::Unavailable,
        subscription: SubscriptionState::Unavailable,
        retry_attempt: server.restarts(),
        discovered: false,
        failure,
    };
    if let Some(client) = client {
        describe_connection(&mut snapshot, &client);
    }
    snapshot
}

pub(crate) fn health_failure(
    required: bool,
    connection: ConnectionState,
    last_error: Option<String>,
) -> Option<String> {
    match classify(connection) {
        Status::Disabled => required.then(|| DISABLED_REQUIRED.to_owned()),
        Status::Failed => Some(last_error.unwrap_or_else(|| FAILED_WITHOUT_DETAIL.to_owned())),
        Status::Connecting | Status::Ready | Status::Unavailable => None,
    }
}

fn describe_connection(snapshot: &mut ServerSnapshot, client: &McpClient) {
    let info = client.server_info();
    let capabilities = info.capabilities;
    snapshot.negotiated_name = info.name.as_deref().map(|name| safe(name, NAME_BYTES));
    snapshot.negotiated_version = info
        .version
        .as_deref()
        .map(|version| safe(version, VERSION_BYTES));
    snapshot.protocol_version = Some(safe(info.protocol_version, VERSION_BYTES));
    snapshot.counts = CapabilityCounts {
        tools: Some(client.tool_catalog().tools.len()),
        resources: capability_count(capabilities.resources.is_some(), false, 0),
        resource_templates: capability_count(capabilities.resources.is_some(), false, 0),
        prompts: capability_count(capabilities.prompts.is_some(), false, 0),
    };
    snapshot.cache_freshness = if client.tools_stale.load(Ordering::Acquire) {
        CacheFreshness::Stale
    } else {
        CacheFreshness::Fresh
    };
    let subscribed = capabilities.tools_list_changed
        || capabilities
            .resources
            .is_some_and(|resources| resources.list_changed)
        || capabilities
            .prompts
            .is_some_and(|prompts| prompts.list_changed);
    snapshot.subscription = match (subscribed, client.transport.listening()) {
        (false, _) => SubscriptionState::Unavailable,
        (true, true) => SubscriptionState::Active,
        (true, false) => SubscriptionState::Stopped,
    };
    snapshot.discovered = true;
}

fn authentication(config: &McpServerConfig) -> AuthenticationState {
    if config.auth.is_some() || config.bearer_token_env.is_some() || !config.header_env.is_empty() {
        AuthenticationState::Configured
    } else {
        AuthenticationState::None
    }
}

fn safe(text: &str, limit: usize) -> String {
    encode_terminal_safe(text.as_bytes(), limit).text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_follow_the_classified_connection() {
        assert_eq!(
            health_failure(true, ConnectionState::Disabled, None).as_deref(),
            Some(DISABLED_REQUIRED)
        );
        assert_eq!(health_failure(false, ConnectionState::Disabled, None), None);
        assert_eq!(
            health_failure(false, ConnectionState::Failed, Some("boom".to_owned())).as_deref(),
            Some("boom")
        );
        assert_eq!(
            health_failure(false, ConnectionState::Failed, None).as_deref(),
            Some(FAILED_WITHOUT_DETAIL)
        );
        assert_eq!(
            health_failure(true, ConnectionState::Disconnected, None),
            None
        );
    }

    #[test]
    fn idle_servers_report_disabled_or_disconnected_with_their_configuration() {
        let mut config = McpServerConfig::stdio("tool\u{1b}", "/bin/true", Vec::new());
        config.bearer_token_env = Some("TOKEN".to_owned());
        let server = Server::new(
            config.clone(),
            crate::server_transport::ConnectOptions::default(),
            std::sync::Arc::default(),
        );
        let snapshot = snapshot_server(&server);
        assert_eq!(snapshot.configured_name, "tool\\x1b");
        assert_eq!(snapshot.connection, ConnectionState::Disconnected);
        assert_eq!(snapshot.authentication, AuthenticationState::Configured);
        assert_eq!(snapshot.counts, CapabilityCounts::default());
        assert!(!snapshot.discovered);
        config.enabled = false;
        config.required = true;
        let disabled = Server::new(
            config,
            crate::server_transport::ConnectOptions::default(),
            std::sync::Arc::default(),
        );
        let snapshot = snapshot_server(&disabled);
        assert_eq!(snapshot.connection, ConnectionState::Disabled);
        assert_eq!(snapshot.failure.as_deref(), Some(DISABLED_REQUIRED));
    }
}
