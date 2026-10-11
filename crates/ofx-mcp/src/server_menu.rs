use ofx_contract::{
    McpMenuAdmission, McpMenuAuthentication, McpMenuConnection, McpMenuCounts, McpMenuServer,
    McpMenuSource, McpMenuStatus, McpMenuTransport,
};

use crate::health::{AuthenticationState, ConnectionState, Status};
use crate::mcp_contract::{ConfigSource, TransportType, WorkspaceAdmission};
use crate::server_lifecycle::Server;
use crate::server_views::snapshot_server;

pub(crate) fn menu_server(server: &Server) -> McpMenuServer {
    let snapshot = snapshot_server(server);
    let config = &server.config;
    McpMenuServer {
        name: config.name.clone(),
        source: match config.source {
            ConfigSource::Profile => McpMenuSource::Profile,
            ConfigSource::Workspace => McpMenuSource::Project,
            ConfigSource::Acp => McpMenuSource::Acp,
        },
        admission: config.workspace_admission.map(|admission| match admission {
            WorkspaceAdmission::Pending => McpMenuAdmission::Pending,
            WorkspaceAdmission::Approved => McpMenuAdmission::Approved,
            WorkspaceAdmission::Rejected => McpMenuAdmission::Rejected,
        }),
        transport: match config.transport {
            TransportType::Stdio => McpMenuTransport::Stdio,
            TransportType::Http => McpMenuTransport::Http,
            TransportType::Sse => McpMenuTransport::Sse,
        },
        required: config.required,
        connection: match snapshot.connection {
            ConnectionState::Disconnected => McpMenuConnection::Disconnected,
            ConnectionState::Disabled => McpMenuConnection::Disabled,
            ConnectionState::Connecting => McpMenuConnection::Connecting,
            ConnectionState::Ready => McpMenuConnection::Ready,
            ConnectionState::Failed => McpMenuConnection::Failed,
        },
        authentication: match snapshot.authentication {
            AuthenticationState::None => McpMenuAuthentication::None,
            AuthenticationState::Configured => McpMenuAuthentication::Configured,
            AuthenticationState::Authenticated => McpMenuAuthentication::Authenticated,
            AuthenticationState::Required => McpMenuAuthentication::Required,
        },
        status: match snapshot.status() {
            Status::Disabled => McpMenuStatus::Disabled,
            Status::Connecting => McpMenuStatus::Connecting,
            Status::Ready => McpMenuStatus::Ready,
            Status::NeedsAuth => McpMenuStatus::NeedsAuth,
            Status::Failed => McpMenuStatus::Failed,
            Status::Unavailable => McpMenuStatus::Unavailable,
        },
        reloading: server.reload_pending(),
        protocol_version: snapshot.protocol_version,
        counts: McpMenuCounts {
            tools: snapshot.counts.tools,
            resources: snapshot.counts.resources,
            resource_templates: snapshot.counts.resource_templates,
            prompts: snapshot.counts.prompts,
        },
        failure: snapshot.failure,
    }
}
