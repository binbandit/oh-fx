use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use ofx_auth::{
    MISSING_CHATGPT_CREDENTIAL_MESSAGE, StoredLogin, codex_login_saved, grok_login_saved,
    provider_route_name, stored_codex_login,
};
use ofx_cli::OutputFormat;
use ofx_config::{
    ConfigDiagnostic, ConnectionError, ProfilePaths, ProviderDefinition, ProviderId,
    SelectionError, Settings, SettingsError,
};
use ofx_mcp::{
    LocalConfigInspection, ProjectMcpChoices, inspect_local_config, profile_config_path,
};

use crate::output_contracts::status::{AuthStatus, Credential, SavedLogins, StatusReport};

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

pub(crate) struct StatusSources<'a> {
    pub(crate) paths: Option<&'a ProfilePaths>,
    pub(crate) home: Option<&'a Path>,
    pub(crate) lookup: Lookup<'a>,
    pub(crate) host_managed: bool,
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
        let mcp = self.mcp(Some(settings), &workspace_root);
        Ok(StatusReport {
            model,
            model_origin: settings.model_origin(&provider, lookup),
            model_source: connection.map_or_else(
                || provider_route_name(&provider).to_owned(),
                |connection| connection.id().to_owned(),
            ),
            connection: connection.map(|connection| connection.id().to_owned()),
            provider_endpoint: connection.map(|connection| connection.base_url().to_owned()),
            auth: self.auth(&provider, connection),
            permission_mode: settings.permission_mode(lookup),
            workspace_root,
            agent_step_limit: settings.max_agent_steps(lookup),
            mcp,
        })
    }

    pub(crate) fn mcp(
        &self,
        settings: Option<&Settings>,
        workspace_root: &Path,
    ) -> LocalConfigInspection {
        let choices = settings.and_then(|settings| {
            ProjectMcpChoices::parse(settings.workspace_entry(), &mut Vec::new()).ok()
        });
        let profile_path = self.paths.map(profile_config_path);
        inspect_local_config(
            profile_path.as_deref(),
            workspace_root,
            choices.as_ref(),
            self.lookup,
        )
    }

    pub(crate) fn auth(
        &self,
        provider: &ProviderId,
        connection: Option<&ProviderDefinition>,
    ) -> AuthStatus {
        if self.host_managed {
            let logins = SavedLogins {
                codex: true,
                grok: false,
            };
            return active(Credential::HostManaged, logins);
        }
        let data = self.paths.map(|paths| paths.data.clone());
        let logins = SavedLogins {
            codex: data.clone().is_some_and(codex_login_saved),
            grok: data.clone().is_some_and(grok_login_saved),
        };
        if let Some(connection) = connection {
            return match connection.resolve(self.lookup, self.home) {
                Err(error @ ConnectionError::MissingCredentials) => {
                    missing(error.to_string(), logins)
                }
                _ => active(Credential::Connection, logins),
            };
        }
        if *provider != ProviderId::Codex {
            let unavailable = SelectionError::ProviderUnavailable(provider.label().to_owned());
            return missing(unavailable.to_string(), logins);
        }
        match data.map_or(StoredLogin::Missing, stored_codex_login) {
            StoredLogin::Saved { expired } => active(
                Credential::Codex { expired },
                SavedLogins {
                    codex: true,
                    ..logins
                },
            ),
            StoredLogin::Unusable(Some(error)) => missing(error.notice().to_owned(), logins),
            StoredLogin::Missing | StoredLogin::Unusable(None) => {
                missing(MISSING_CHATGPT_CREDENTIAL_MESSAGE.to_owned(), logins)
            }
        }
    }
}

fn active(credential: Credential, logins: SavedLogins) -> AuthStatus {
    AuthStatus {
        active: Some(credential),
        help: None,
        logins,
    }
}

fn missing(help: String, logins: SavedLogins) -> AuthStatus {
    AuthStatus {
        active: None,
        help: Some(help),
        logins,
    }
}
