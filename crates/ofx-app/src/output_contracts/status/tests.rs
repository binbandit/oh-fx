use ofx_mcp::{
    ConfigScope, ConfigSource, ConfiguredServer, ProfileConfigWarning, ProfileConfigWarningCause,
    TransportType,
};

use super::*;

fn inspection(profile_diagnostic: ProfileConfigDiagnostic) -> LocalConfigInspection {
    LocalConfigInspection {
        profile_diagnostic,
        servers: vec![ConfiguredServer {
            name: "tool".to_owned(),
            source: ConfigSource::Workspace,
            scope: ConfigScope::Workspace,
            workspace_admission: Some(WorkspaceAdmission::Approved),
            required: true,
            transport: TransportType::Http,
        }],
        configuration_issues: vec!["skipped\u{1b}".to_owned()],
        inspection_error: None,
    }
}

fn report(auth: AuthStatus, mcp: LocalConfigInspection) -> StatusReport {
    StatusReport {
        model: "model-a".to_owned(),
        model_origin: "default",
        model_source: "local".to_owned(),
        connection: Some("local".to_owned()),
        provider_endpoint: None,
        auth,
        permission_mode: PermissionMode::Yolo,
        workspace_root: PathBuf::from("/work"),
        agent_step_limit: 9,
        mcp,
    }
}

fn missing() -> AuthStatus {
    AuthStatus {
        active: None,
        help: Some("sign in".to_owned()),
        logins: SavedLogins::default(),
    }
}

#[test]
fn the_text_snapshot_follows_upstreams_line_order_and_encodes_untrusted_text() {
    let warning = ProfileConfigWarning::new(
        ProfileConfigWarningCause::SuspiciousServerKey,
        Some("MCP\u{7}"),
        2,
    );
    let text = report(
        missing(),
        inspection(ProfileConfigDiagnostic::Warning(warning)),
    )
    .render(OutputFormat::Text);
    assert_eq!(
        text,
        "[status] model=model-a\n[status] model_origin=default\n[status] model_source=local\n[status] mcp_config_warning=suspicious_server_key key=MCP\\x07 additional_matches=2\n[status] auth=missing\n[status] connected_providers=none\n[status] auth_refreshable=false\n[status] auth_help=sign in\n[status] permission_mode=full access\n[status] workspace=/work\n[status] history_turns=0\n[status] session_permission_grants=0\n[status] agent_step_limit=9\n[status] mcp_connection_check=not_checked\n[status] mcp_servers=1 mcp_configuration_issues=1\n[status] mcp_server=tool source=workspace scope=workspace admission=approved transport=http connection=not_checked authentication=not_checked\n[status] mcp_configuration_issue=skipped\\x1b\n"
    );
}

#[test]
fn the_json_snapshot_keeps_raw_values_and_wire_labels() {
    let auth = AuthStatus {
        active: Some(Credential::Codex { expired: true }),
        help: None,
        logins: SavedLogins {
            codex: true,
            grok: false,
        },
    };
    let json = report(
        auth,
        inspection(ProfileConfigDiagnostic::Failed("StreamTooLong".to_owned())),
    )
    .render(OutputFormat::Json);
    assert_eq!(
        json,
        "{\"kind\":\"status\",\"model\":\"model-a\",\"model_origin\":\"default\",\"model_source\":\"local\",\"mcp_config_error\":\"StreamTooLong\",\"auth\":\"Codex subscription\",\"connected_providers\":[\"codex\"],\"auth_refreshable\":true,\"auth_expired\":true,\"permission_mode\":\"yolo\",\"workspace\":\"/work\",\"history_turns\":0,\"session_permission_grants\":0,\"agent_step_limit\":9,\"mcp\":{\"connection_check\":\"not_checked\",\"servers\":[{\"name\":\"tool\",\"source\":\"workspace\",\"scope\":\"workspace\",\"admission\":\"approved\",\"required\":true,\"transport\":\"http\",\"connection\":\"not_checked\",\"authentication\":\"not_checked\"}],\"configuration_issues\":[\"skipped\\u001b\"],\"inspection_error\":null}}\n"
    );
}

#[test]
fn an_active_connection_is_listed_before_the_codex_login() {
    let auth = AuthStatus {
        active: Some(Credential::Connection),
        help: None,
        logins: SavedLogins {
            codex: true,
            grok: false,
        },
    };
    let status = report(auth, inspection(ProfileConfigDiagnostic::Clear));
    assert!(
        status
            .render(OutputFormat::Text)
            .contains("\n[status] connected_providers=local, Codex\n")
    );
    assert!(
        status
            .render(OutputFormat::Json)
            .contains(",\"connected_providers\":[\"local\",\"codex\"],")
    );
}
