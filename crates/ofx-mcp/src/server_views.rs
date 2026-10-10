use std::sync::Arc;

use ofx_text::encode_terminal_safe;

use crate::catalog_freshness::{Freshness, SnapshotMetadata, effective_freshness};
use crate::feature_catalog::FeatureCatalogs;
use crate::features::prompts::Prompt;
use crate::features::resources::{Resource, ResourceTemplate};
use crate::health::{
    AuthenticationState, CacheFreshness, CapabilityCounts, ConnectionState, ServerSnapshot, Status,
    SubscriptionState, capability_count, classify, observed_connection, retry_delay,
};
use crate::mcp_contract::{McpServerConfig, TransportType};
use crate::model_catalog::{ServerSummary, classify_availability};
use crate::operation_control::monotonic_millis;
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
    let (connection, client, last_error) = observe(server);
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
        retry_in_ms: None,
        discovered: false,
        failure,
    };
    if let Some(client) = client {
        describe_connection(&mut snapshot, &client, &server.features);
    }
    snapshot
}

pub(crate) fn model_summary(server: &Server) -> ServerSummary {
    let (connection, client, _) = observe(server);
    ServerSummary {
        name: server.config.name.clone(),
        availability: classify_availability(connection),
        tool_count: client
            .filter(|_| connection == ConnectionState::Ready)
            .map(|client| client.tool_catalog().tools.len()),
    }
}

fn observe(server: &Server) -> (ConnectionState, Option<Arc<McpClient>>, Option<String>) {
    let config = &server.config;
    match server.lifecycle() {
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
    }
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

fn describe_connection(
    snapshot: &mut ServerSnapshot,
    client: &McpClient,
    features: &FeatureCatalogs,
) {
    let info = client.server_info();
    let capabilities = info.capabilities;
    snapshot.negotiated_name = info.name.as_deref().map(|name| safe(name, NAME_BYTES));
    snapshot.negotiated_version = info
        .version
        .as_deref()
        .map(|version| safe(version, VERSION_BYTES));
    snapshot.protocol_version = Some(safe(info.protocol_version, VERSION_BYTES));
    let resources = features.snapshot::<Resource>();
    let templates = features.snapshot::<ResourceTemplate>();
    let prompts = features.snapshot::<Prompt>();
    let advertises_resources = capabilities.resources.is_some();
    let tools = client.tool_snapshot();
    snapshot.counts = CapabilityCounts {
        tools: Some(tools.catalog.tools.len()),
        resources: capability_count(
            advertises_resources,
            resources.is_some(),
            resources.as_ref().map_or(0, |catalog| catalog.items.len()),
        ),
        resource_templates: capability_count(
            advertises_resources,
            templates.is_some(),
            templates.as_ref().map_or(0, |catalog| catalog.items.len()),
        ),
        prompts: capability_count(
            capabilities.prompts.is_some(),
            prompts.is_some(),
            prompts.as_ref().map_or(0, |catalog| catalog.items.len()),
        ),
    };
    let tools_invalidated = client.tools_invalidation.pending();
    let catalogs = [
        Some(tools.metadata),
        resources.map(|catalog| catalog.metadata),
        templates.map(|catalog| catalog.metadata),
        prompts.map(|catalog| catalog.metadata),
    ];
    let now_ms = monotonic_millis();
    snapshot.cache_freshness = catalogs
        .iter()
        .flatten()
        .map(|metadata| cache_freshness(*metadata, now_ms, tools_invalidated))
        .fold(CacheFreshness::Unavailable, CacheFreshness::max);
    snapshot.retry_attempt = catalogs
        .iter()
        .flatten()
        .map(|metadata| metadata.refresh_attempt)
        .fold(snapshot.retry_attempt, u8::max);
    snapshot.retry_in_ms = retry_delay(
        catalogs
            .iter()
            .flatten()
            .map(|metadata| metadata.retry_at_ms)
            .filter(|retry_at| *retry_at != 0)
            .min(),
        now_ms,
    );
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

fn cache_freshness(metadata: SnapshotMetadata, now_ms: u64, invalidated: bool) -> CacheFreshness {
    match effective_freshness(metadata, now_ms, invalidated) {
        Freshness::Fresh => CacheFreshness::Fresh,
        Freshness::Stale => CacheFreshness::Stale,
        Freshness::Refreshing => CacheFreshness::Refreshing,
        Freshness::FailedRefresh => CacheFreshness::FailedRefresh,
    }
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

    #[tokio::test]
    async fn a_stdio_change_subscription_stays_active_after_its_process_ends() {
        let script = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[]}'; exit 0 ;;
  esac
done
"#;
        let server = Arc::new(Server::new(
            McpServerConfig::stdio(
                "fixture",
                "/bin/sh",
                vec!["-c".to_owned(), script.to_owned()],
            ),
            crate::server_transport::ConnectOptions::default(),
            Arc::default(),
        ));
        server.start().await;
        let Lifecycle::Ready(client) = server.lifecycle() else {
            panic!("the server is not ready");
        };
        for _ in 0..200 {
            if !client.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let snapshot = snapshot_server(&server);
        assert_eq!(snapshot.connection, ConnectionState::Failed);
        assert_eq!(snapshot.subscription, SubscriptionState::Active);
        server.stop(crate::transport::ShutdownMode::Immediate).await;
    }

    #[test]
    fn idle_servers_report_disabled_or_disconnected_with_their_configuration() {
        let mut config = McpServerConfig::stdio("tool\u{1b}", "/bin/true", Vec::new());
        config.bearer_token_env = Some("TOKEN".to_owned());
        let server = Server::new(
            config.clone(),
            crate::server_transport::ConnectOptions::default(),
            Arc::default(),
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
            Arc::default(),
        );
        let snapshot = snapshot_server(&disabled);
        assert_eq!(snapshot.connection, ConnectionState::Disabled);
        assert_eq!(snapshot.failure.as_deref(), Some(DISABLED_REQUIRED));
    }
}
