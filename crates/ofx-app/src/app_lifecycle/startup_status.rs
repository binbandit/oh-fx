use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use ofx_auth::{
    MISSING_CHATGPT_CREDENTIAL_MESSAGE, StoredLogin, codex_login_saved, provider_route_name,
    stored_codex_login,
};
use ofx_cli::OutputFormat;
use ofx_config::{
    ConfigDiagnostic, ConnectionError, ProfilePaths, ProviderDefinition, ProviderId,
    SelectionError, Settings, SettingsError,
};
use ofx_mcp::{ProjectMcpChoices, inspect_local_config, profile_config_path};

use crate::output_contracts::status::{AuthStatus, Credential, StatusReport};

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Debug, thiserror::Error)]
pub enum StartupStatusError {
    #[error("WorkspaceUnavailable")]
    WorkspaceUnavailable,
    #[error("{0}")]
    Settings(#[from] SettingsError),
    #[error("{0}")]
    Selection(#[from] SelectionError),
}

pub struct StartupStatus {
    diagnostics: Vec<ConfigDiagnostic>,
    report: StatusReport,
}

impl StartupStatus {
    pub fn load(host_managed: bool) -> Result<Self, StartupStatusError> {
        let workspace_root = env::current_dir()
            .and_then(fs::canonicalize)
            .map_err(|_| StartupStatusError::WorkspaceUnavailable)?;
        let paths = ProfilePaths::from_environment();
        let settings = match &paths {
            Some(paths) => Settings::load(paths, &workspace_root)?,
            None => Settings::default(),
        };
        let home = env::home_dir();
        let lookup = |name: &str| env::var(name).ok();
        let sources = StatusSources {
            paths: paths.as_ref(),
            home: home.as_deref(),
            lookup: &lookup,
            host_managed,
        };
        let report = sources.report(&settings, workspace_root)?;
        Ok(Self {
            diagnostics: settings.diagnostics().to_vec(),
            report,
        })
    }

    pub fn diagnostics(&self) -> &[ConfigDiagnostic] {
        &self.diagnostics
    }

    pub fn render(&self, format: OutputFormat) -> String {
        self.report.render(format)
    }
}

struct StatusSources<'a> {
    paths: Option<&'a ProfilePaths>,
    home: Option<&'a Path>,
    lookup: Lookup<'a>,
    host_managed: bool,
}

impl StatusSources<'_> {
    fn report(
        &self,
        settings: &Settings,
        workspace_root: PathBuf,
    ) -> Result<StatusReport, SelectionError> {
        let lookup = self.lookup;
        let provider = settings.selected_provider(lookup)?;
        let (model, connection) = if provider == ProviderId::Codex {
            (settings.selected_codex_model(None, lookup)?, None)
        } else {
            let connection = settings.selected_connection(lookup)?;
            (
                settings.selected_model(connection, None, lookup)?,
                Some(connection),
            )
        };
        let choices = ProjectMcpChoices::parse(settings.workspace_entry(), &mut Vec::new()).ok();
        let profile_path = self.paths.map(profile_config_path);
        let mcp = inspect_local_config(
            profile_path.as_deref(),
            &workspace_root,
            choices.as_ref(),
            lookup,
        );
        Ok(StatusReport {
            model,
            model_origin: settings.model_origin(&provider, lookup),
            model_source: connection.map_or_else(
                || provider_route_name(&provider).to_owned(),
                |connection| connection.id().to_owned(),
            ),
            connection: connection.map(|connection| connection.id().to_owned()),
            provider_endpoint: connection.map(|connection| connection.base_url().to_owned()),
            auth: self.auth(connection),
            permission_mode: settings.permission_mode(lookup),
            workspace_root,
            agent_step_limit: settings.max_agent_steps(lookup),
            mcp,
        })
    }

    fn auth(&self, connection: Option<&ProviderDefinition>) -> AuthStatus {
        if self.host_managed {
            return active(Credential::HostManaged, true);
        }
        let data = self.paths.map(|paths| paths.data.clone());
        let codex_connected = data.clone().is_some_and(codex_login_saved);
        if let Some(connection) = connection {
            return match connection.resolve(self.lookup, self.home) {
                Err(error @ ConnectionError::MissingCredentials) => {
                    missing(error.to_string(), codex_connected)
                }
                _ => active(Credential::Connection, codex_connected),
            };
        }
        match data.map_or(StoredLogin::Missing, stored_codex_login) {
            StoredLogin::Saved { expired } => active(Credential::Codex { expired }, true),
            StoredLogin::Unusable(Some(error)) => {
                missing(error.notice().to_owned(), codex_connected)
            }
            StoredLogin::Missing | StoredLogin::Unusable(None) => missing(
                MISSING_CHATGPT_CREDENTIAL_MESSAGE.to_owned(),
                codex_connected,
            ),
        }
    }
}

fn active(credential: Credential, codex_connected: bool) -> AuthStatus {
    AuthStatus {
        active: Some(credential),
        help: None,
        codex_connected,
    }
}

fn missing(help: String, codex_connected: bool) -> AuthStatus {
    AuthStatus {
        active: None,
        help: Some(help),
        codex_connected,
    }
}
