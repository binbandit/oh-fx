use std::fmt::Write as _;

use ofx_text::encode_terminal_safe;

use crate::mcp_contract::{ConfigScope, ConfigSource, TransportType, WorkspaceAdmission};

const MAX_PENDING_SUMMARY_NAMES: usize = 4;
const SUMMARY_NAME_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionState {
    Disconnected,
    Disabled,
    Connecting,
    Ready,
    Failed,
}

impl ConnectionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Disabled => "disabled",
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

pub(crate) fn observed_connection(
    published: ConnectionState,
    transport_running: Option<bool>,
) -> ConnectionState {
    if published == ConnectionState::Ready && transport_running == Some(false) {
        return ConnectionState::Failed;
    }
    published
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthenticationState {
    None,
    Configured,
}

impl AuthenticationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Configured => "configured",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Disabled,
    Connecting,
    Ready,
    Failed,
    Unavailable,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Unavailable => "unavailable",
        }
    }
}

pub(crate) fn classify(connection: ConnectionState) -> Status {
    match connection {
        ConnectionState::Disabled => Status::Disabled,
        ConnectionState::Connecting => Status::Connecting,
        ConnectionState::Ready => Status::Ready,
        ConnectionState::Failed => Status::Failed,
        ConnectionState::Disconnected => Status::Unavailable,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheFreshness {
    Unavailable,
    Fresh,
    Stale,
}

impl CacheFreshness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Fresh => "fresh",
            Self::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriptionState {
    Unavailable,
    Active,
    Stopped,
}

impl SubscriptionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Active => "active",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CapabilityCounts {
    pub tools: Option<usize>,
    pub resources: Option<usize>,
    pub resource_templates: Option<usize>,
    pub prompts: Option<usize>,
}

pub(crate) fn capability_count(advertised: bool, loaded: bool, count: usize) -> Option<usize> {
    if !advertised {
        return Some(0);
    }
    loaded.then_some(count)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerSnapshot {
    pub configured_name: String,
    pub source: ConfigSource,
    pub scope: ConfigScope,
    pub workspace_admission: Option<WorkspaceAdmission>,
    pub required: bool,
    pub transport: TransportType,
    pub negotiated_name: Option<String>,
    pub negotiated_version: Option<String>,
    pub protocol_version: Option<String>,
    pub connection: ConnectionState,
    pub authentication: AuthenticationState,
    pub counts: CapabilityCounts,
    pub cache_freshness: CacheFreshness,
    pub subscription: SubscriptionState,
    pub retry_attempt: u8,
    pub discovered: bool,
    pub failure: Option<String>,
}

impl ServerSnapshot {
    pub(crate) fn status(&self) -> Status {
        classify(self.connection)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub servers: Vec<ServerSnapshot>,
    pub configuration_issues: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupDecision {
    Ready,
    Degraded,
    Blocked,
}

pub(crate) fn startup_decision(servers: &[ServerSnapshot]) -> StartupDecision {
    let mut degraded = false;
    for server in servers {
        let status = server.status();
        if status == Status::Ready || status == Status::Disabled && !server.required {
            continue;
        }
        if server.required {
            return StartupDecision::Blocked;
        }
        degraded = true;
    }
    if degraded {
        StartupDecision::Degraded
    } else {
        StartupDecision::Ready
    }
}

pub(crate) fn render(snapshot: &Snapshot) -> String {
    if snapshot.servers.is_empty() && snapshot.configuration_issues.is_empty() {
        return "No MCP servers configured.\n".to_owned();
    }
    let mut out = String::new();
    if !snapshot.servers.is_empty() {
        let _ = writeln!(
            out,
            "MCP health ({} {}):",
            snapshot.servers.len(),
            plural(snapshot.servers.len(), "server", "servers")
        );
    }
    for server in &snapshot.servers {
        let _ = writeln!(
            out,
            "  {} source={} scope={} policy={} transport={} state={} auth={} status={}",
            server.configured_name,
            source_label(server.source),
            scope_label(server.scope),
            if server.required {
                "required"
            } else {
                "optional"
            },
            server.transport.as_str(),
            server.connection.as_str(),
            server.authentication.as_str(),
            server.status().as_str(),
        );
        if let Some(admission) = server.workspace_admission {
            let _ = writeln!(out, "    admission={}", admission_label(admission));
        }
        let _ = writeln!(
            out,
            "    negotiated_name={} negotiated_version={} protocol={}",
            server.negotiated_name.as_deref().unwrap_or("unavailable"),
            server
                .negotiated_version
                .as_deref()
                .unwrap_or("unavailable"),
            server.protocol_version.as_deref().unwrap_or("unavailable"),
        );
        let _ = writeln!(
            out,
            "    tools={} resources={} templates={} prompts={} cache={} subscription={}",
            count_label(server.counts.tools),
            count_label(server.counts.resources),
            count_label(server.counts.resource_templates),
            count_label(server.counts.prompts),
            server.cache_freshness.as_str(),
            server.subscription.as_str(),
        );
        let _ = writeln!(
            out,
            "    retry_attempt={} retry_in_ms=none discovery={}",
            server.retry_attempt,
            if server.discovered {
                "completed"
            } else {
                "pending"
            },
        );
        if let Some(failure) = &server.failure {
            let _ = writeln!(out, "    failure={failure}");
        }
    }
    if !snapshot.configuration_issues.is_empty() {
        out.push_str("Project MCP configuration errors:\n");
        for issue in &snapshot.configuration_issues {
            let _ = writeln!(out, "  {issue}");
        }
    }
    out
}

pub(crate) fn render_summary(snapshot: &Snapshot) -> String {
    if snapshot.servers.is_empty() && snapshot.configuration_issues.is_empty() {
        return "MCP: no servers configured. Use /mcp add <name> <command> [args...].".to_owned();
    }
    let issues = snapshot.configuration_issues.len();
    if snapshot.servers.is_empty() {
        return format!(
            "MCP: {issues} project .mcp.json {}. Use /mcp list for details.",
            plural(issues, "error", "errors")
        );
    }
    let mut ready = 0;
    let mut connecting = 0;
    let mut failed = 0;
    let mut pending = Vec::new();
    for server in &snapshot.servers {
        if server.source == ConfigSource::Workspace
            && server.workspace_admission == Some(WorkspaceAdmission::Pending)
        {
            pending.push(server.configured_name.as_str());
        }
        match server.status() {
            Status::Ready => ready += 1,
            Status::Connecting => connecting += 1,
            Status::Failed => failed += 1,
            Status::Disabled | Status::Unavailable => {}
        }
    }
    let count = snapshot.servers.len();
    let mut out = format!(
        "MCP: {count} {} — {ready} ready, {connecting} connecting, 0 needs auth, {failed} failed.",
        plural(count, "server", "servers")
    );
    if !pending.is_empty() {
        out.push_str(" Pending approval: ");
        let shown = pending.len().min(MAX_PENDING_SUMMARY_NAMES);
        for (index, name) in pending[..shown].iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&encode_terminal_safe(name.as_bytes(), SUMMARY_NAME_BYTES).text);
        }
        if pending.len() > shown {
            let _ = write!(out, ", +{} more", pending.len() - shown);
        }
        out.push('.');
    }
    if issues > 0 {
        let _ = write!(out, " Project .mcp.json errors: {issues}.");
    }
    out.push_str(" Use /mcp list for details.");
    out
}

pub(crate) fn render_startup_notice(snapshot: &Snapshot) -> Option<String> {
    let failed = snapshot
        .servers
        .iter()
        .filter(|server| server.status() == Status::Failed)
        .count();
    if failed == 0 {
        return None;
    }
    let (noun, verb) = if failed == 1 {
        ("server", "needs")
    } else {
        ("servers", "need")
    };
    Some(format!(
        "MCP startup: {failed} {noun} {verb} attention, {failed} failed. Use /mcp list for details."
    ))
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

fn count_label(count: Option<usize>) -> String {
    count.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

fn source_label(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::Profile => "profile",
        ConfigSource::Acp => "acp",
        ConfigSource::Workspace => "workspace",
    }
}

fn scope_label(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Profile => "profile",
        ConfigScope::AcpSession => "acp_session",
        ConfigScope::Workspace => "workspace",
    }
}

fn admission_label(admission: WorkspaceAdmission) -> &'static str {
    match admission {
        WorkspaceAdmission::Pending => "pending",
        WorkspaceAdmission::Approved => "approved",
        WorkspaceAdmission::Rejected => "rejected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str) -> ServerSnapshot {
        ServerSnapshot {
            configured_name: name.to_owned(),
            source: ConfigSource::Profile,
            scope: ConfigScope::Profile,
            workspace_admission: None,
            required: false,
            transport: TransportType::Stdio,
            negotiated_name: None,
            negotiated_version: None,
            protocol_version: None,
            connection: ConnectionState::Disconnected,
            authentication: AuthenticationState::None,
            counts: CapabilityCounts::default(),
            cache_freshness: CacheFreshness::Unavailable,
            subscription: SubscriptionState::Unavailable,
            retry_attempt: 0,
            discovered: false,
            failure: None,
        }
    }

    fn snapshot(servers: Vec<ServerSnapshot>) -> Snapshot {
        Snapshot {
            servers,
            configuration_issues: Vec::new(),
        }
    }

    #[test]
    fn required_health_blocks_while_optional_health_degrades() {
        let mut failed = server("one");
        failed.connection = ConnectionState::Failed;
        assert_eq!(
            startup_decision(std::slice::from_ref(&failed)),
            StartupDecision::Degraded
        );
        failed.required = true;
        assert_eq!(startup_decision(&[failed]), StartupDecision::Blocked);
        let mut disabled = server("off");
        disabled.connection = ConnectionState::Disabled;
        assert_eq!(
            startup_decision(std::slice::from_ref(&disabled)),
            StartupDecision::Ready
        );
        disabled.required = true;
        assert_eq!(startup_decision(&[disabled]), StartupDecision::Blocked);
    }

    #[test]
    fn health_capability_counts_distinguish_unadvertised_and_unloaded_features() {
        assert_eq!(capability_count(true, false, 0), None);
        assert_eq!(capability_count(false, false, 0), Some(0));
        assert_eq!(capability_count(true, true, 7), Some(7));
    }

    #[test]
    fn health_derives_failed_connections_from_transport_state() {
        assert_eq!(
            observed_connection(ConnectionState::Ready, Some(false)),
            ConnectionState::Failed
        );
        assert_eq!(
            observed_connection(ConnectionState::Ready, Some(true)),
            ConnectionState::Ready
        );
        assert_eq!(
            observed_connection(ConnectionState::Ready, None),
            ConnectionState::Ready
        );
    }

    #[test]
    fn health_rendering_includes_complete_typed_state_without_secret_bearing_configuration() {
        let mut entry = server("server\\u001b[31m");
        entry.negotiated_name = Some("fixture".to_owned());
        entry.negotiated_version = Some("1.2.3".to_owned());
        entry.protocol_version = Some("2025-11-25".to_owned());
        entry.required = true;
        entry.transport = TransportType::Http;
        entry.connection = ConnectionState::Failed;
        entry.authentication = AuthenticationState::Configured;
        entry.counts = CapabilityCounts {
            tools: Some(2),
            resources: Some(3),
            resource_templates: Some(4),
            prompts: None,
        };
        entry.cache_freshness = CacheFreshness::Stale;
        entry.subscription = SubscriptionState::Stopped;
        entry.retry_attempt = 2;
        entry.discovered = true;
        entry.failure = Some("Connection or discovery failed.".to_owned());
        let output = render(&snapshot(vec![entry]));
        assert_eq!(
            output,
            "MCP health (1 server):\n  server\\u001b[31m source=profile scope=profile policy=required transport=http state=failed auth=configured status=failed\n    negotiated_name=fixture negotiated_version=1.2.3 protocol=2025-11-25\n    tools=2 resources=3 templates=4 prompts=unknown cache=stale subscription=stopped\n    retry_attempt=2 retry_in_ms=none discovery=completed\n    failure=Connection or discovery failed.\n"
        );
    }

    #[test]
    fn health_rendering_names_workspace_admission_and_configuration_errors() {
        let mut entry = server("docs");
        entry.source = ConfigSource::Workspace;
        entry.scope = ConfigScope::Workspace;
        entry.workspace_admission = Some(WorkspaceAdmission::Pending);
        let output = render(&Snapshot {
            servers: vec![entry],
            configuration_issues: vec!["Project MCP server 'db' is invalid.".to_owned()],
        });
        assert!(output.contains(
            "  docs source=workspace scope=workspace policy=optional transport=stdio state=disconnected auth=none status=unavailable\n    admission=pending\n"
        ));
        assert!(output.ends_with(
            "Project MCP configuration errors:\n  Project MCP server 'db' is invalid.\n"
        ));
        assert_eq!(render(&Snapshot::default()), "No MCP servers configured.\n");
    }

    #[test]
    fn compact_health_summary_reports_actionable_aggregate_state() {
        assert_eq!(
            render_summary(&Snapshot::default()),
            "MCP: no servers configured. Use /mcp add <name> <command> [args...]."
        );
        let mut ready = server("ready");
        ready.connection = ConnectionState::Ready;
        let mut loading = server("loading");
        loading.connection = ConnectionState::Connecting;
        let mut broken = server("broken");
        broken.connection = ConnectionState::Failed;
        let mut off = server("off");
        off.connection = ConnectionState::Disabled;
        assert_eq!(
            render_summary(&snapshot(vec![ready, loading, broken, off])),
            "MCP: 4 servers — 1 ready, 1 connecting, 0 needs auth, 1 failed. Use /mcp list for details."
        );
    }

    #[test]
    fn compact_health_summary_names_pending_workspace_servers() {
        let workspace = |name: &str, admission| {
            let mut entry = server(name);
            entry.source = ConfigSource::Workspace;
            entry.scope = ConfigScope::Workspace;
            entry.workspace_admission = Some(admission);
            entry
        };
        let summary = render_summary(&snapshot(vec![
            workspace("docs", WorkspaceAdmission::Pending),
            workspace("db\u{1b}", WorkspaceAdmission::Pending),
            workspace("approved", WorkspaceAdmission::Approved),
        ]));
        assert!(
            summary.contains("Pending approval: docs, db\\x1b."),
            "{summary}"
        );
        assert!(!summary.contains("approved"));
        let many: Vec<_> = (0..6)
            .map(|index| workspace(&format!("s{index}"), WorkspaceAdmission::Pending))
            .collect();
        assert!(
            render_summary(&snapshot(many)).contains("Pending approval: s0, s1, s2, s3, +2 more.")
        );
    }

    #[test]
    fn compact_health_summary_counts_project_configuration_errors() {
        let issues = Snapshot {
            servers: Vec::new(),
            configuration_issues: vec!["one".to_owned(), "two".to_owned()],
        };
        assert_eq!(
            render_summary(&issues),
            "MCP: 2 project .mcp.json errors. Use /mcp list for details."
        );
        let mixed = Snapshot {
            servers: vec![server("plain")],
            configuration_issues: vec!["one".to_owned()],
        };
        assert_eq!(
            render_summary(&mixed),
            "MCP: 1 server — 0 ready, 0 connecting, 0 needs auth, 0 failed. Project .mcp.json errors: 1. Use /mcp list for details."
        );
    }

    #[test]
    fn startup_notice_names_failures_and_stays_quiet_when_healthy() {
        let mut healthy = server("up");
        healthy.connection = ConnectionState::Ready;
        assert_eq!(render_startup_notice(&snapshot(vec![healthy])), None);
        let mut broken = server("down");
        broken.connection = ConnectionState::Failed;
        assert_eq!(
            render_startup_notice(&snapshot(vec![broken.clone()])).as_deref(),
            Some("MCP startup: 1 server needs attention, 1 failed. Use /mcp list for details.")
        );
        assert_eq!(
            render_startup_notice(&snapshot(vec![broken.clone(), broken])).as_deref(),
            Some("MCP startup: 2 servers need attention, 2 failed. Use /mcp list for details.")
        );
    }
}
