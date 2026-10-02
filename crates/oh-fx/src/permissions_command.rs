use std::io::{self, Write};
use std::process::ExitCode;

use ofx_app::Profile;
use ofx_cli::{OutputFormat, TopLevelKind};
use ofx_contract::PermissionMode;
use serde_json::json;

pub(crate) fn run(format: OutputFormat) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let profile = match Profile::load() {
        Ok(profile) => profile,
        Err(error) => {
            let _ = writeln!(io::stderr(), "oh-fx: {error}");
            return ExitCode::FAILURE;
        }
    };
    let settings = profile.settings();
    let mut stderr = io::stderr().lock();
    for diagnostic in settings.diagnostics() {
        let _ = writeln!(stderr, "oh-fx: {diagnostic}");
    }
    drop(stderr);
    crate::print(
        render(settings.permission_mode(), format).as_bytes(),
        crate::command_write_failure(TopLevelKind::Permissions),
    )
}

fn render(mode: PermissionMode, format: OutputFormat) -> String {
    match format {
        OutputFormat::Text => format!(
            "[permissions] mode={}\n[permissions] configured rules: (none)\n[permissions] session grants: (none)\n",
            mode.display_label()
        ),
        OutputFormat::Json => {
            let mut line = json!({
                "kind": "permissions",
                "mode": mode.label(),
                "grant_count": 0,
                "grant_scope": "session",
                "runtime_grants_available": false,
                "rules_scope": "persistent_config",
                "rules": [],
                "grants": [],
            })
            .to_string();
            line.push('\n');
            line
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_snapshot_names_the_mode_and_reports_no_rules_or_grants() {
        assert_eq!(
            render(PermissionMode::Yolo, OutputFormat::Text),
            "[permissions] mode=full access\n[permissions] configured rules: (none)\n[permissions] session grants: (none)\n"
        );
        assert!(
            render(PermissionMode::Ask, OutputFormat::Text).starts_with("[permissions] mode=ask\n")
        );
    }

    #[test]
    fn the_json_snapshot_uses_the_saved_mode_label_and_upstreams_field_order() {
        assert_eq!(
            render(PermissionMode::Yolo, OutputFormat::Json),
            "{\"kind\":\"permissions\",\"mode\":\"yolo\",\"grant_count\":0,\"grant_scope\":\"session\",\"runtime_grants_available\":false,\"rules_scope\":\"persistent_config\",\"rules\":[],\"grants\":[]}\n"
        );
        assert!(render(PermissionMode::Auto, OutputFormat::Json).contains("\"mode\":\"auto\""));
    }
}
