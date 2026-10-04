use std::path::Path;

use crate::mcp_contract::{ConfigSource, McpServerConfig, ProfileConfigWarning};
use crate::profile_store::{ProfileStoreError, load_profile_document};
use crate::project_config::{
    ProfileParseResult, ProjectMcpChoices, WorkspaceDiagnostic, merge_native,
};
use crate::startup_admission::{StartupDecision, StartupPhase, decide_startup};
use crate::workspace_config::load_workspace_config_with_environment;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeConfigLoad {
    pub configs: Vec<McpServerConfig>,
    pub profile_warning: Option<ProfileConfigWarning>,
    pub workspace_diagnostics: Vec<WorkspaceDiagnostic>,
}

pub fn load_native_configs(
    profile_path: Option<&Path>,
    workspace_root: &Path,
    choices: Option<&ProjectMcpChoices>,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Result<NativeConfigLoad, ProfileStoreError> {
    let profile = match profile_path {
        Some(path) => load_profile_document(path)?,
        None => ProfileParseResult::default(),
    };
    let Some(choices) = choices else {
        return Ok(NativeConfigLoad {
            configs: profile.configs,
            profile_warning: profile.diagnostic,
            workspace_diagnostics: Vec::new(),
        });
    };
    let workspace = load_workspace_config_with_environment(workspace_root, choices, environment)?;
    Ok(NativeConfigLoad {
        configs: merge_native(profile.configs, workspace.configs),
        profile_warning: profile.diagnostic,
        workspace_diagnostics: workspace.diagnostics,
    })
}

pub fn preview_workspace_authority(
    workspace_root: &Path,
    choices: Option<&ProjectMcpChoices>,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<String>, ProfileStoreError> {
    let Some(choices) = choices else {
        return Ok(Vec::new());
    };
    let workspace = load_workspace_config_with_environment(workspace_root, choices, environment)?;
    let mut names: Vec<String> = Vec::new();
    for config in workspace.configs {
        if config.source == ConfigSource::Workspace
            && decide_startup(&config, StartupPhase::All) == StartupDecision::Connect
            && !names.contains(&config.name)
        {
            names.push(config.name);
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::mcp_contract::{ConfigSource, WorkspaceAdmission};

    #[test]
    fn profile_entries_win_and_trusted_workspace_entries_expand() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let profile = home.path().join("mcp.json");
        fs::write(&profile, r#"{"mcp":{"shared":{"command":"profile"}}}"#).unwrap();
        fs::write(
            workspace.path().join(".mcp.json"),
            r#"{"mcpServers":{"shared":{"command":"workspace"},"tool":{"command":"${TOOL}"},"waiting":{"command":"x"}}}"#,
        )
        .unwrap();
        let choices = ProjectMcpChoices {
            approved: vec!["tool".to_owned(), "shared".to_owned()],
            ..ProjectMcpChoices::default()
        };
        let lookup = |name: &str| (name == "TOOL").then(|| "node".to_owned());
        let load =
            load_native_configs(Some(&profile), workspace.path(), Some(&choices), &lookup).unwrap();
        let summary: Vec<_> = load
            .configs
            .iter()
            .map(|config| {
                (
                    config.name.as_str(),
                    config.command.as_deref().unwrap_or_default(),
                    config.workspace_admission,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("shared", "profile", None),
                ("tool", "node", Some(WorkspaceAdmission::Approved)),
                ("waiting", "x", Some(WorkspaceAdmission::Pending)),
            ]
        );
        assert_eq!(load.configs[1].source, ConfigSource::Workspace);
        assert!(load.workspace_diagnostics.is_empty());
    }

    #[test]
    fn unreadable_trust_choices_keep_only_profile_servers() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join(".mcp.json"),
            r#"{"mcpServers":{"tool":{"command":"node"}}}"#,
        )
        .unwrap();
        let profile = home.path().join("mcp.json");
        let load = load_native_configs(Some(&profile), workspace.path(), None, &|_| None).unwrap();
        assert!(load.configs.is_empty());
        let without_home = load_native_configs(
            None,
            workspace.path(),
            Some(&ProjectMcpChoices::default()),
            &|_| None,
        )
        .unwrap();
        assert_eq!(without_home.configs.len(), 1);
        assert_eq!(
            without_home.configs[0].workspace_admission,
            Some(WorkspaceAdmission::Pending)
        );
    }

    #[test]
    fn workspace_authority_names_the_approved_project_servers_that_may_start() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join(".mcp.json"),
            r#"{"mcpServers":{"docs":{"command":"node"},"db":{"command":"node"},"off":{"command":"node","enabled":false}}}"#,
        )
        .unwrap();
        let choices = ProjectMcpChoices {
            approved: vec!["docs".to_owned(), "off".to_owned()],
            rejected: vec!["db".to_owned()],
            ..ProjectMcpChoices::default()
        };
        assert_eq!(
            preview_workspace_authority(workspace.path(), Some(&choices), &|_| None).unwrap(),
            ["docs"]
        );
        let everything = ProjectMcpChoices {
            enable_all: true,
            ..ProjectMcpChoices::default()
        };
        assert_eq!(
            preview_workspace_authority(workspace.path(), Some(&everything), &|_| None).unwrap(),
            ["docs", "db"]
        );
        assert!(
            preview_workspace_authority(workspace.path(), None, &|_| None)
                .unwrap()
                .is_empty()
        );
    }
}
