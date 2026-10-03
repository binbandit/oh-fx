use ofx_mcp::ProfileConfigDiagnostic;

use super::*;
use crate::output_contracts::status::{Credential, SavedLogins};

fn report(checks: Vec<Check>) -> DoctorReport {
    DoctorReport {
        workspace_root: PathBuf::from("/work\u{1b}[2J"),
        model: "gpt-5.4".to_owned(),
        model_source: Some("Codex subscription".to_owned()),
        auth: AuthStatus {
            active: Some(Credential::Codex { expired: true }),
            help: None,
            logins: SavedLogins::default(),
        },
        permission_mode: PermissionMode::Yolo,
        agent_step_limit: 3,
        checks,
        mcp: LocalConfigInspection {
            profile_diagnostic: ProfileConfigDiagnostic::Clear,
            servers: Vec::new(),
            configuration_issues: Vec::new(),
            inspection_error: None,
        },
    }
}

fn checks() -> Vec<Check> {
    vec![
        Check {
            name: "workspace",
            status: CheckStatus::Ok,
            detail: "using workspace /work".to_owned(),
        },
        Check {
            name: "auth",
            status: CheckStatus::Warn,
            detail: "Codex subscription is configured; session expired; refreshable=true"
                .to_owned(),
        },
        Check {
            name: "config",
            status: CheckStatus::Fail,
            detail: "bad\u{7}".to_owned(),
        },
    ]
}

#[test]
fn the_text_report_counts_checks_and_encodes_untrusted_text() {
    assert_eq!(
        report(checks()).render(OutputFormat::Text),
        "[doctor] ok=1 warn=1 fail=1\n[doctor] workspace=/work\\x1b[2J\n[doctor] model=gpt-5.4\n[doctor] model_source=Codex subscription\n[doctor] auth=Codex subscription\n[doctor] auth_refreshable=true\n[doctor] auth_expired=true\n[doctor] permission_mode=full access\n[doctor] agent_step_limit=3\n[doctor] mcp_connection_check=not_checked\n[doctor] mcp_servers=0 mcp_configuration_issues=0\n[ok] workspace: using workspace /work\n[warn] auth: Codex subscription is configured; session expired; refreshable=true\n[fail] config: bad\\x07\n"
    );
}

#[test]
fn the_json_report_keeps_upstreams_field_order_and_raw_values() {
    assert_eq!(
        report(checks()).render(OutputFormat::Json),
        "{\"kind\":\"doctor\",\"ok_count\":1,\"warn_count\":1,\"fail_count\":1,\"workspace\":\"/work\\u001b[2J\",\"model\":\"gpt-5.4\",\"model_source\":\"Codex subscription\",\"auth\":\"Codex subscription\",\"auth_refreshable\":true,\"auth_expired\":true,\"permission_mode\":\"yolo\",\"agent_step_limit\":3,\"checks\":[{\"name\":\"workspace\",\"status\":\"ok\",\"detail\":\"using workspace /work\"},{\"name\":\"auth\",\"status\":\"warn\",\"detail\":\"Codex subscription is configured; session expired; refreshable=true\"},{\"name\":\"config\",\"status\":\"fail\",\"detail\":\"bad\\u0007\"}],\"mcp\":{\"connection_check\":\"not_checked\",\"servers\":[],\"configuration_issues\":[],\"inspection_error\":null}}\n"
    );
}
