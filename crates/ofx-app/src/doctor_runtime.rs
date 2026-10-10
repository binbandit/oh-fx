use std::env;
use std::fs;
use std::path::Path;

use ofx_auth::provider_route_name;
use ofx_cli::OutputFormat;
use ofx_config::{ProfilePaths, ProviderId, SelectionError, Settings};
use ofx_contract::PermissionMode;
use ofx_mcp::ProfileConfigDiagnostic;
use ofx_session::{DoctorDiagnostic, DoctorIssueKind, SessionStore};

use crate::app_lifecycle::startup_status::StatusSources;
use crate::output_contracts::doctor::{Check, CheckStatus, DoctorReport};
use crate::output_contracts::status::AuthStatus;

const USER_SETTINGS: &str = "~/.config/oh-fx/settings.json";
const PROJECT_SETTINGS: &str = ".oh-fx.json";
const PROFILE_MCP: &str = "~/.config/oh-fx/mcp.json";
const SESSIONS_DIRECTORY: &str = "sessions";
const UNUSABLE_PROFILE: &str = "InvalidProfileConfiguration";
const SESSION_DIAGNOSTICS_LIMIT: usize = 64;

pub struct Doctor {
    report: DoctorReport,
}

impl Doctor {
    pub fn collect(host_managed: bool) -> Result<Self, &'static str> {
        let workspace_root = env::current_dir()
            .and_then(fs::canonicalize)
            .map_err(|_| "WorkspaceUnavailable")?;
        let paths = ProfilePaths::from_environment();
        ofx_trace::configure_from_env(
            &workspace_root,
            paths.as_ref().map(|paths| paths.state.as_path()),
        );
        let home = env::home_dir();
        let lookup = |name: &str| env::var(name).ok();
        let sources = StatusSources {
            paths: paths.as_ref(),
            home: home.as_deref(),
            lookup: &lookup,
            host_managed,
        };
        let report = Collection {
            sources: &sources,
            paths: paths.as_ref(),
            workspace_root: &workspace_root,
            checks: vec![check(
                "workspace",
                CheckStatus::Ok,
                format!("using workspace {}", workspace_root.display()),
            )],
        }
        .run();
        Ok(Self { report })
    }

    pub fn render(&self, format: OutputFormat) -> String {
        self.report.render(format)
    }
}

struct Collection<'a> {
    sources: &'a StatusSources<'a>,
    paths: Option<&'a ProfilePaths>,
    workspace_root: &'a Path,
    checks: Vec<Check>,
}

struct Resolved {
    model: String,
    model_source: Option<String>,
    auth: AuthStatus,
    permission_mode: PermissionMode,
    agent_step_limit: u64,
}

impl Collection<'_> {
    fn run(mut self) -> DoctorReport {
        let lookup = self.sources.lookup;
        let loaded = match self.paths {
            Some(paths) => {
                Settings::load(paths, self.workspace_root).map_err(|error| error.to_string())
            }
            None => Ok(Settings::default()),
        }
        .and_then(|settings| match settings.selected_provider(lookup) {
            Ok(provider) => Ok((settings, provider)),
            Err(error) => Err(error.to_string()),
        });
        let resolved = match &loaded {
            Ok((settings, provider)) => self.loaded(settings, provider),
            Err(error) => self.unloaded(error),
        };
        self.state_checks();
        self.git_check();
        self.gh_check();
        let mcp = self.sources.mcp(
            loaded.as_ref().ok().map(|(settings, _)| settings),
            self.workspace_root,
        );
        let mut checks = self.checks;
        if let Some(position) = checks.iter().position(|check| check.name == "auth")
            && let Some(mcp_check) = mcp_check(&mcp.profile_diagnostic)
        {
            checks.insert(position, mcp_check);
        }
        DoctorReport {
            workspace_root: self.workspace_root.to_owned(),
            model: resolved.model,
            model_source: resolved.model_source,
            auth: resolved.auth,
            permission_mode: resolved.permission_mode,
            agent_step_limit: resolved.agent_step_limit,
            checks,
            mcp,
        }
    }

    fn unloaded(&mut self, error: &str) -> Resolved {
        let auth = self.sources.auth(&ProviderId::Gateway, None);
        self.push(
            "config",
            CheckStatus::Fail,
            format!("failed to load config: {error}"),
        );
        self.auth_check(&auth);
        self.push(
            "startup",
            CheckStatus::Fail,
            format!("failed to resolve startup settings: {error}"),
        );
        Resolved {
            model: String::new(),
            model_source: None,
            auth,
            permission_mode: PermissionMode::Auto,
            agent_step_limit: 0,
        }
    }

    fn loaded(&mut self, settings: &Settings, provider: &ProviderId) -> Resolved {
        let lookup = self.sources.lookup;
        let connection = settings.selected_connection(lookup).ok();
        let auth = self.sources.auth(provider, connection);
        self.config_checks(settings);
        match connection.map(|connection| connection.resolve(lookup, self.sources.home)) {
            Some(Err(error)) => self.push("auth", CheckStatus::Fail, error.to_string()),
            _ => self.auth_check(&auth),
        }
        let permission_mode = settings.permission_mode(lookup);
        let agent_step_limit = settings.max_agent_steps(lookup);
        let selected = match connection {
            _ if settings.profile_is_unusable() => Err(UNUSABLE_PROFILE.to_owned()),
            Some(connection) => settings
                .selected_model(connection, None, lookup)
                .map_err(|error| error.to_string()),
            None if *provider == ProviderId::Codex => settings
                .selected_codex_model(None, lookup)
                .map_err(|error| error.to_string()),
            None => {
                Err(SelectionError::ProviderUnavailable(provider.label().to_owned()).to_string())
            }
        };
        let model = match selected {
            Ok(model) => {
                self.push(
                    "startup",
                    CheckStatus::Ok,
                    format!(
                        "resolved model={model}, permission_mode={}, agent_step_limit={agent_step_limit}",
                        permission_mode.display_label()
                    ),
                );
                model
            }
            Err(error) => {
                self.push("startup", CheckStatus::Fail, error);
                String::new()
            }
        };
        Resolved {
            model,
            model_source: (*provider != ProviderId::Gateway).then(|| {
                connection.map_or_else(
                    || provider_route_name(provider).to_owned(),
                    |connection| connection.id().to_owned(),
                )
            }),
            auth,
            permission_mode,
            agent_step_limit,
        }
    }

    fn config_checks(&mut self, settings: &Settings) {
        let user = self
            .paths
            .is_some_and(|paths| Settings::user_file(paths).is_file())
            && !settings.user_layer_rejected();
        let project = Settings::project_file(self.workspace_root).is_file()
            && !settings.project_layer_rejected();
        let present: Vec<&str> = [(user, USER_SETTINGS), (project, PROJECT_SETTINGS)]
            .into_iter()
            .filter_map(|(present, name)| present.then_some(name))
            .collect();
        if present.is_empty() {
            self.push(
                "config",
                CheckStatus::Warn,
                "no config files found; using defaults and env overrides".to_owned(),
            );
        } else {
            self.push(
                "config",
                CheckStatus::Ok,
                format!("loaded config from {}", present.join(", ")),
            );
        }
        for diagnostic in settings.diagnostics() {
            self.push("config", CheckStatus::Warn, diagnostic.doctor_detail());
        }
    }

    fn auth_check(&mut self, auth: &AuthStatus) {
        if let Some(help) = &auth.help {
            self.push("auth", CheckStatus::Fail, help.clone());
            return;
        }
        let expired = if auth.expired() {
            "; session expired"
        } else {
            ""
        };
        let status = if auth.expired() {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        };
        self.push(
            "auth",
            status,
            format!(
                "{} is configured{expired}; refreshable={}",
                auth.label(),
                auth.refreshable()
            ),
        );
    }

    fn state_checks(&mut self) {
        let Some(paths) = self.paths else {
            self.push(
                "state",
                CheckStatus::Warn,
                "failed to inspect workspace state: HomeNotSet".to_owned(),
            );
            return;
        };
        let root = self.workspace_root.to_string_lossy();
        let store = match SessionStore::open_read_only(&paths.data, &root) {
            Ok(store) => store,
            Err(error) => {
                self.push(
                    "state",
                    CheckStatus::Fail,
                    format!("failed to inspect workspace state: {error}"),
                );
                return;
            }
        };
        if !store.is_initialized() {
            self.push(
                "state",
                CheckStatus::Warn,
                "durable state is not initialized".to_owned(),
            );
            self.push(
                "sessions",
                CheckStatus::Warn,
                "no saved sessions yet".to_owned(),
            );
            return;
        }
        let sessions = paths.data.join(SESSIONS_DIRECTORY);
        self.push(
            "state",
            CheckStatus::Ok,
            format!(
                "state dir ready at {} (per-session managed state created on demand)",
                sessions.display()
            ),
        );
        if let Ok(inspection) = store.inspect_for_doctor(SESSION_DIAGNOSTICS_LIMIT) {
            for diagnostic in &inspection.diagnostics {
                let (status, detail) = session_diagnostic(diagnostic, &sessions);
                self.push("session", status, detail);
            }
            if inspection.truncated {
                let count = inspection.inspected_count;
                self.push(
                    "session",
                    CheckStatus::Warn,
                    format!(
                        "session diagnostics truncated after {count} session director{} to keep doctor bounded",
                        if count == 1 { "y" } else { "ies" }
                    ),
                );
            }
        }
        match store.catalog() {
            Ok(catalog) => match catalog.summaries().first() {
                Some(latest) => self.push(
                    "sessions",
                    CheckStatus::Ok,
                    format!(
                        "{} saved session(s); latest={}",
                        catalog.summaries().len(),
                        latest.id
                    ),
                ),
                None => {
                    self.push(
                        "sessions",
                        CheckStatus::Warn,
                        "no saved sessions yet".to_owned(),
                    );
                }
            },
            Err(_) => self.push(
                "sessions",
                CheckStatus::Warn,
                "exact saved-session count unavailable without a full session scan".to_owned(),
            ),
        }
    }

    fn git_check(&mut self) {
        if self.workspace_root.join(".git").exists() {
            self.push(
                "git",
                CheckStatus::Ok,
                "git metadata detected for this workspace".to_owned(),
            );
        } else {
            self.push(
                "git",
                CheckStatus::Warn,
                "not a git repository; pr/issue workflows will be limited".to_owned(),
            );
        }
    }

    fn gh_check(&mut self) {
        let found = env::var_os("PATH").is_some_and(|path| {
            env::split_paths(&path)
                .filter(|entry| !entry.as_os_str().is_empty())
                .any(|entry| entry.join("gh").exists())
        });
        if found {
            self.push("gh", CheckStatus::Ok, "GitHub CLI found in PATH".to_owned());
        } else {
            self.push(
                "gh",
                CheckStatus::Warn,
                "GitHub CLI not found in PATH; publish workflows unavailable".to_owned(),
            );
        }
    }

    fn push(&mut self, name: &'static str, status: CheckStatus, detail: String) {
        self.checks.push(check(name, status, detail));
    }
}

fn session_diagnostic(diagnostic: &DoctorDiagnostic, sessions: &Path) -> (CheckStatus, String) {
    let id = &diagnostic.session_id;
    let sessions = sessions.display();
    let (status, report_only, recovery) = match diagnostic.kind {
        DoctorIssueKind::AuthorityTransitionPending => (
            CheckStatus::Warn,
            " report_only=true",
            "rerun oh-fx doctor after active writers exit; cleanup is guarded".to_owned(),
        ),
        DoctorIssueKind::CanonicalStateInvalid => (
            CheckStatus::Fail,
            "",
            format!("back up {sessions}, then inspect this session with oh-fx session {id} --json"),
        ),
        DoctorIssueKind::UnsafePath => (
            CheckStatus::Fail,
            "",
            format!("back up {sessions} and avoid opening this session until the path is repaired"),
        ),
    };
    (
        status,
        format!(
            "session {id}: {}{report_only}; recovery={recovery}",
            diagnostic.kind.name()
        ),
    )
}

fn mcp_check(diagnostic: &ProfileConfigDiagnostic) -> Option<Check> {
    match diagnostic {
        ProfileConfigDiagnostic::Clear => None,
        ProfileConfigDiagnostic::Warning(warning) => {
            let key = warning
                .key
                .as_ref()
                .map_or_else(String::new, |key| format!(" key={key}"));
            Some(check(
                "mcp_config",
                CheckStatus::Warn,
                format!(
                    "{PROFILE_MCP} warning: {}{key} additional_matches={}",
                    warning.cause.as_str(),
                    warning.additional_matches
                ),
            ))
        }
        ProfileConfigDiagnostic::Failed(error) => Some(check(
            "mcp_config",
            CheckStatus::Fail,
            format!("failed to load {PROFILE_MCP}: {error}"),
        )),
    }
}

fn check(name: &'static str, status: CheckStatus, detail: String) -> Check {
    Check {
        name,
        status,
        detail,
    }
}
