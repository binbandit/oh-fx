use std::io;
use std::path::Path;

use ofx_text::encode_terminal_safe;

use crate::mcp_contract::{
    ConfigScope, ConfigSource, McpServerConfig, ProfileConfigWarning, TransportType,
    WorkspaceAdmission,
};
use crate::profile_store::{ProfileStoreError, load_profile_document};
use crate::project_config::{
    ProjectMcpChoices, WorkspaceDiagnostic, merge_native, render_workspace_diagnostic,
};
use crate::workspace_config::load_workspace_config_with_environment;

const MAX_SERVER_NAME_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileConfigDiagnostic {
    Clear,
    Warning(ProfileConfigWarning),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredServer {
    pub name: String,
    pub source: ConfigSource,
    pub scope: ConfigScope,
    pub workspace_admission: Option<WorkspaceAdmission>,
    pub required: bool,
    pub transport: TransportType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfigInspection {
    pub profile_diagnostic: ProfileConfigDiagnostic,
    pub servers: Vec<ConfiguredServer>,
    pub configuration_issues: Vec<String>,
    pub inspection_error: Option<String>,
}

impl LocalConfigInspection {
    fn new(
        configs: &[McpServerConfig],
        diagnostics: &[WorkspaceDiagnostic],
        profile_diagnostic: ProfileConfigDiagnostic,
        inspection_error: Option<String>,
    ) -> Self {
        Self {
            profile_diagnostic,
            servers: configs.iter().map(ConfiguredServer::from).collect(),
            configuration_issues: diagnostics
                .iter()
                .map(render_workspace_diagnostic)
                .collect(),
            inspection_error,
        }
    }
}

impl From<&McpServerConfig> for ConfiguredServer {
    fn from(config: &McpServerConfig) -> Self {
        Self {
            name: encode_terminal_safe(config.name.as_bytes(), MAX_SERVER_NAME_BYTES).text,
            source: config.source,
            scope: config.scope,
            workspace_admission: config.workspace_admission,
            required: config.required,
            transport: config.transport,
        }
    }
}

pub fn inspect_local_config(
    profile_path: Option<&Path>,
    workspace_root: &Path,
    choices: Option<&ProjectMcpChoices>,
    environment: &dyn Fn(&str) -> Option<String>,
) -> LocalConfigInspection {
    let profile = match profile_path.map(load_profile_document).transpose() {
        Ok(profile) => profile.unwrap_or_default(),
        Err(error) => {
            let name = profile_error_name(&error);
            return LocalConfigInspection::new(
                &[],
                &[],
                ProfileConfigDiagnostic::Failed(name.clone()),
                Some(name),
            );
        }
    };
    let diagnostic = profile.diagnostic.map_or(
        ProfileConfigDiagnostic::Clear,
        ProfileConfigDiagnostic::Warning,
    );
    let Some(choices) = choices else {
        return LocalConfigInspection::new(&profile.configs, &[], diagnostic, None);
    };
    match load_workspace_config_with_environment(workspace_root, choices, environment) {
        Ok(workspace) => LocalConfigInspection::new(
            &merge_native(profile.configs, workspace.configs),
            &workspace.diagnostics,
            diagnostic,
            None,
        ),
        Err(error) => {
            LocalConfigInspection::new(&[], &[], diagnostic, Some(io_error_name(&error).to_owned()))
        }
    }
}

fn profile_error_name(error: &ProfileStoreError) -> String {
    match error {
        ProfileStoreError::Io(error) => io_error_name(error).to_owned(),
        other => other.to_string(),
    }
}

fn io_error_name(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "AccessDenied",
        io::ErrorKind::IsADirectory => "IsDir",
        io::ErrorKind::NotADirectory => "NotDir",
        io::ErrorKind::NotFound => "FileNotFound",
        _ => "Unexpected",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::mcp_contract::ProfileConfigWarningCause;

    fn server(
        name: &str,
        source: ConfigSource,
        scope: ConfigScope,
        workspace_admission: Option<WorkspaceAdmission>,
    ) -> ConfiguredServer {
        ConfiguredServer {
            name: name.to_owned(),
            source,
            scope,
            workspace_admission,
            required: false,
            transport: TransportType::Stdio,
        }
    }

    #[test]
    fn profile_and_workspace_servers_merge_with_their_admission_and_skipped_entries() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let profile = home.path().join("mcp.json");
        fs::write(
            &profile,
            r#"{"mcp":{"docs":{"command":"node"}},"mcpServers":{"x":{"command":"y"}}}"#,
        )
        .unwrap();
        fs::write(
            workspace.path().join(".mcp.json"),
            r#"{"mcpServers":{"tool":{"command":"x"},"bad":7,"esc\u001b":{"command":"z"}}}"#,
        )
        .unwrap();
        let inspection = inspect_local_config(
            Some(&profile),
            workspace.path(),
            Some(&ProjectMcpChoices::default()),
            &|_| None,
        );
        assert_eq!(
            inspection.servers,
            [
                server("docs", ConfigSource::Profile, ConfigScope::Profile, None),
                server(
                    "tool",
                    ConfigSource::Workspace,
                    ConfigScope::Workspace,
                    Some(WorkspaceAdmission::Pending)
                ),
                server(
                    "esc\\x1b",
                    ConfigSource::Workspace,
                    ConfigScope::Workspace,
                    Some(WorkspaceAdmission::Pending)
                ),
            ]
        );
        assert_eq!(
            inspection.configuration_issues,
            [".mcp.json server 'bad' was skipped: invalid_entry."]
        );
        assert_eq!(
            inspection.profile_diagnostic,
            ProfileConfigDiagnostic::Warning(ProfileConfigWarning::new(
                ProfileConfigWarningCause::IgnoredMcpServersAlias,
                Some("mcpServers"),
                0
            ))
        );
        assert_eq!(inspection.inspection_error, None);
    }

    #[test]
    fn unreadable_choices_keep_only_profile_servers() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let profile = home.path().join("mcp.json");
        fs::write(&profile, r#"{"mcp":{"docs":{"command":"node"}}}"#).unwrap();
        fs::write(
            workspace.path().join(".mcp.json"),
            r#"{"mcpServers":{"tool":{"command":"x"}}}"#,
        )
        .unwrap();
        let inspection = inspect_local_config(Some(&profile), workspace.path(), None, &|_| None);
        assert_eq!(
            inspection.servers,
            [server(
                "docs",
                ConfigSource::Profile,
                ConfigScope::Profile,
                None
            )]
        );
        assert_eq!(
            inspection.profile_diagnostic,
            ProfileConfigDiagnostic::Clear
        );
    }

    #[test]
    fn a_broken_profile_file_fails_the_whole_inspection_with_its_error_name() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join(".mcp.json"),
            r#"{"mcpServers":{"tool":{"command":"x"}}}"#,
        )
        .unwrap();
        let profile = home.path().join("mcp.json");
        fs::write(&profile, "{").unwrap();
        let choices = ProjectMcpChoices::default();
        let inspection =
            inspect_local_config(Some(&profile), workspace.path(), Some(&choices), &|_| None);
        assert_eq!(
            inspection,
            LocalConfigInspection {
                profile_diagnostic: ProfileConfigDiagnostic::Failed(
                    "McpConfigInvalidJson".to_owned()
                ),
                servers: Vec::new(),
                configuration_issues: Vec::new(),
                inspection_error: Some("McpConfigInvalidJson".to_owned()),
            }
        );
        let directory =
            inspect_local_config(Some(home.path()), workspace.path(), Some(&choices), &|_| {
                None
            });
        assert_eq!(directory.inspection_error.as_deref(), Some("IsDir"));
        let without_profile =
            inspect_local_config(None, workspace.path(), Some(&choices), &|_| None);
        assert_eq!(without_profile.servers.len(), 1);
    }
}
