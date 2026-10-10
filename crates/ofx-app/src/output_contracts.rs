use std::fmt::Write as _;
use std::path::Path;

use ofx_auth::provider_route_name;
use ofx_config::{ProviderDefinition, ProviderId};
use ofx_contract::PermissionMode;

use crate::app_bootstrap_runtime::CredentialSource;

pub(crate) mod doctor;
pub(crate) mod sessions;
pub(crate) mod status;
pub(crate) mod usage;
pub(crate) mod workspace;

const CODEX_CONNECTION: &str = "Codex";

pub(crate) struct StatusSnapshot<'a> {
    pub(crate) model: &'a str,
    pub(crate) connection: Option<&'a ProviderDefinition>,
    pub(crate) source: CredentialSource,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) workspace_root: &'a Path,
    pub(crate) history_turns: usize,
    pub(crate) session_permission_grants: usize,
    pub(crate) agent_step_limit: u64,
    pub(crate) ultrafast_requested: bool,
}

impl StatusSnapshot<'_> {
    pub(crate) fn render_interactive_body(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "model={}", self.model);
        match self.connection {
            Some(connection) => {
                let _ = writeln!(out, "model_source={}", connection.id());
                let _ = writeln!(out, "provider_endpoint={}", connection.base_url());
            }
            None => {
                let _ = writeln!(
                    out,
                    "model_source={}",
                    provider_route_name(&ProviderId::Codex)
                );
            }
        }
        let _ = writeln!(out, "auth={}", self.source.label());
        let connected = self
            .connection
            .map_or(CODEX_CONNECTION, ProviderDefinition::id);
        let _ = writeln!(out, "connected_providers={connected}");
        let _ = writeln!(out, "auth_refreshable={}", self.source.refreshable());
        let _ = writeln!(
            out,
            "permission_mode={}",
            self.permission_mode.display_label()
        );
        let _ = writeln!(out, "workspace={}", self.workspace_root.display());
        let _ = writeln!(out, "history_turns={}", self.history_turns);
        let _ = writeln!(
            out,
            "session_permission_grants={}",
            self.session_permission_grants
        );
        let _ = writeln!(out, "agent_step_limit={}", self.agent_step_limit);
        let _ = write!(out, "ultrafast_requested={}", self.ultrafast_requested);
        out
    }
}

#[cfg(test)]
mod tests {
    use ofx_config::{ProfilePaths, Settings};

    use super::*;

    fn connection(directory: &Path) -> ProviderDefinition {
        let config = directory.join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("settings.json"),
            r#"{"provider":"local","providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://127.0.0.1:8080/v1/","auth":{"type":"none"}}}}"#,
        )
        .unwrap();
        let paths = ProfilePaths {
            config,
            data: directory.join("data"),
            state: directory.join("state"),
            cache: directory.join("cache"),
        };
        Settings::load(&paths, directory)
            .unwrap()
            .selected_connection(&|_| None)
            .unwrap()
            .clone()
    }

    #[test]
    fn a_configured_connection_names_its_endpoint_and_cannot_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let connection = connection(directory.path());
        let snapshot = StatusSnapshot {
            model: "model-a",
            connection: Some(&connection),
            source: CredentialSource::Configured,
            permission_mode: PermissionMode::Ask,
            workspace_root: Path::new("/work/space"),
            history_turns: 3,
            session_permission_grants: 2,
            agent_step_limit: 17,
            ultrafast_requested: true,
        };
        assert_eq!(
            snapshot.render_interactive_body(),
            "model=model-a\nmodel_source=local\nprovider_endpoint=http://127.0.0.1:8080/v1\nauth=configured provider\nconnected_providers=local\nauth_refreshable=false\npermission_mode=ask\nworkspace=/work/space\nhistory_turns=3\nsession_permission_grants=2\nagent_step_limit=17\nultrafast_requested=true"
        );
    }

    #[test]
    fn a_codex_subscription_names_its_route_and_refreshes() {
        let snapshot = StatusSnapshot {
            model: "gpt-6.1-sol",
            connection: None,
            source: CredentialSource::Codex,
            permission_mode: PermissionMode::Yolo,
            workspace_root: Path::new("/work"),
            history_turns: 0,
            session_permission_grants: 0,
            agent_step_limit: 0,
            ultrafast_requested: false,
        };
        assert_eq!(
            snapshot.render_interactive_body(),
            "model=gpt-6.1-sol\nmodel_source=Codex subscription\nauth=Codex subscription\nconnected_providers=Codex\nauth_refreshable=true\npermission_mode=full access\nworkspace=/work\nhistory_turns=0\nsession_permission_grants=0\nagent_step_limit=0\nultrafast_requested=false"
        );
    }
}
