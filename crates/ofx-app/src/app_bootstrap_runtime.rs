use std::env;
use std::ffi::OsStr;
use std::fs;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ofx_agent::{Agent, AgentConfig, Approvals, ProjectContext, RuntimeContext};
use ofx_auth::{CHATGPT_RELOGIN_MESSAGE, CHATGPT_SOURCE_LABEL};
use ofx_config::{
    ConfigDiagnostic, ConnectionError, ContextLimitOverride, ProfilePaths, ProviderDefinition,
    ProviderId, SelectionError, Settings, SettingsError, request_output_tokens,
};
use ofx_contract::{
    BoxFuture, CapabilityLookup, CapabilityResolver, ModelCapabilities, ModelProvider,
    PermissionMode, Tool,
};
use ofx_exec::ManagedExecutions;
use ofx_gateway::ChatCompletionsProvider;
use ofx_http::ClientError;
use ofx_permissions::PermissionPolicy;
use tokio_util::sync::CancellationToken;

use crate::codex_provider::{
    CodexUnavailable, DetachedRefreshes, SubscriptionEndpoints, codex_subscription,
};
use crate::context::{
    GATEWAY_SYSTEM_PROMPT, HostProjectContext, HostRuntimeContext, InstructionLimits,
    ProfileLocation, gather_project_context,
};
use crate::tool_set;

const CONFIGURED_SOURCE_LABEL: &str = "configured provider";
const CONFIGURED_SOURCE_REPAIR: &str = "Check the configured provider auth environment variable.";

pub struct Profile {
    workspace_root: PathBuf,
    paths: Option<ProfilePaths>,
    settings: Settings,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("WorkspaceUnavailable")]
    WorkspaceUnavailable,
    #[error("{0}")]
    Settings(#[from] SettingsError),
    #[error("InvalidProfileConfiguration")]
    Unusable(Vec<ConfigDiagnostic>),
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("{0}")]
    Selection(#[from] SelectionError),
    #[error("{0}")]
    Connection(#[from] ConnectionError),
    #[error("{0}")]
    InvalidConnection(ClientError),
    #[error("{0}")]
    Codex(#[from] CodexUnavailable),
    #[error("InvalidModel")]
    InvalidModel(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Configured,
    Codex,
}

impl CredentialSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Configured => CONFIGURED_SOURCE_LABEL,
            Self::Codex => CHATGPT_SOURCE_LABEL,
        }
    }

    pub const fn relogin(self) -> Option<&'static str> {
        match self {
            Self::Configured => None,
            Self::Codex => Some(CHATGPT_RELOGIN_MESSAGE),
        }
    }

    pub(crate) const fn repair(self) -> &'static str {
        match self {
            Self::Configured => CONFIGURED_SOURCE_REPAIR,
            Self::Codex => CHATGPT_RELOGIN_MESSAGE,
        }
    }
}

pub struct Launch<'a> {
    pub model: Option<&'a OsStr>,
    pub permission_mode: PermissionMode,
    pub system_prompt: Option<String>,
    pub reasoning_effort: Option<String>,
    pub fast_mode: bool,
    pub context_limits: &'a [ContextLimitOverride],
    pub command_timeout: Option<Duration>,
    pub executions: &'a ManagedExecutions,
    pub endpoints: SubscriptionEndpoints,
}

pub struct AgentSetup {
    provider: Arc<dyn ModelProvider>,
    configured_model: Option<String>,
    capabilities: Option<Arc<dyn CapabilityResolver>>,
    connection: Option<ProviderDefinition>,
    source: CredentialSource,
    tools: Vec<Arc<dyn Tool>>,
    context: Arc<dyn RuntimeContext>,
    permissions: Arc<PermissionPolicy>,
    approvals: Option<Approvals>,
    refreshes: Option<Arc<DetachedRefreshes>>,
    project: Option<(Arc<HostProjectContext>, ProjectContext)>,
    context_notices: Vec<String>,
    config: AgentConfig,
}

struct Route {
    provider: Arc<dyn ModelProvider>,
    capabilities: Option<Arc<dyn CapabilityResolver>>,
    connection: Option<ProviderDefinition>,
    model: String,
    configured_model: Option<String>,
    source: CredentialSource,
    uses_tls: bool,
}

impl Profile {
    pub fn load() -> Result<Self, ProfileError> {
        let workspace_root = env::current_dir()
            .and_then(fs::canonicalize)
            .map_err(|_| ProfileError::WorkspaceUnavailable)?;
        let paths = ProfilePaths::from_environment();
        let settings = match &paths {
            Some(paths) => Settings::load(paths, &workspace_root)?,
            None => Settings::default(),
        };
        Self::new(workspace_root, paths, settings)
    }

    pub(crate) fn new(
        workspace_root: PathBuf,
        paths: Option<ProfilePaths>,
        settings: Settings,
    ) -> Result<Self, ProfileError> {
        if settings.profile_is_unusable() {
            return Err(ProfileError::Unusable(settings.diagnostics().to_vec()));
        }
        Ok(Self {
            workspace_root,
            paths,
            settings,
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn data_dir(&self) -> Option<&Path> {
        self.paths.as_ref().map(|paths| paths.data.as_path())
    }

    pub fn resume_selection(
        &mut self,
        provider: &ProviderId,
        binding: Option<[u8; 32]>,
        model: &str,
    ) -> Result<(), SelectionError> {
        let lookup = |name: &str| env::var(name).ok();
        self.settings
            .resume_selection(provider, binding, model, &lookup)
    }

    pub async fn connect(
        &self,
        launch: Launch<'_>,
        cancel: &CancellationToken,
    ) -> Result<AgentSetup, ConnectError> {
        self.prepare(launch, false, cancel).await
    }

    pub(crate) async fn connect_interactive(
        &self,
        launch: Launch<'_>,
        cancel: &CancellationToken,
    ) -> Result<AgentSetup, ConnectError> {
        self.prepare(launch, true, cancel).await
    }

    async fn prepare(
        &self,
        launch: Launch<'_>,
        interactive: bool,
        cancel: &CancellationToken,
    ) -> Result<AgentSetup, ConnectError> {
        let refreshes = interactive.then(Arc::default);
        let route = self
            .route(launch.model, launch.endpoints, refreshes.clone(), cancel)
            .await?;
        if route.uses_tls {
            ofx_http::warm_tls_roots();
        }
        let mut project = self.project_context(launch.context_limits);
        let context_notices = project
            .as_mut()
            .map(|(_, snapshot)| mem::take(&mut snapshot.notices))
            .unwrap_or_default();
        let lookup = |name: &str| env::var(name).ok();
        let config = AgentConfig {
            system_prompt: launch
                .system_prompt
                .unwrap_or_else(|| GATEWAY_SYSTEM_PROMPT.to_owned()),
            max_output_tokens: output_tokens(route.connection.as_ref(), &route.model),
            step_limit: self.settings.max_agent_steps(&lookup),
            model: route.model,
            reasoning_effort: launch.reasoning_effort,
            fast_mode: launch.fast_mode,
            auto_compact_percent: self.settings.auto_compact_percent(&lookup),
        };
        let permission_mode = launch.permission_mode;
        Ok(AgentSetup {
            provider: route.provider,
            configured_model: route.configured_model,
            capabilities: route.capabilities,
            connection: route.connection,
            source: route.source,
            tools: tool_set::ask_tools(
                &self.workspace_root,
                launch.executions,
                launch.command_timeout,
                permission_mode,
            ),
            context: Arc::new(HostRuntimeContext::new(
                self.workspace_root.clone(),
                permission_mode,
                interactive,
            )),
            permissions: Arc::new(PermissionPolicy::new(
                permission_mode,
                self.workspace_root.clone(),
            )),
            approvals: interactive.then(Approvals::default),
            refreshes,
            project,
            context_notices,
            config,
        })
    }

    async fn route(
        &self,
        requested: Option<&OsStr>,
        endpoints: SubscriptionEndpoints,
        refreshes: Option<Arc<DetachedRefreshes>>,
        cancel: &CancellationToken,
    ) -> Result<Route, ConnectError> {
        let lookup = |name: &str| env::var(name).ok();
        if self.settings.codex_selected(&lookup)? {
            return self
                .codex_route(requested, endpoints, &lookup, refreshes, cancel)
                .await;
        }
        let connection = self.settings.selected_connection(&lookup)?;
        let model = select_model(requested, |model| {
            self.settings.selected_model(connection, model, &lookup)
        })?;
        let configured_model = self.settings.selected_model(connection, None, &lookup).ok();
        let resolved = connection.resolve(&lookup, env::home_dir().as_deref())?;
        let uses_tls = uses_tls(&resolved.chat_url);
        let provider = ChatCompletionsProvider::new(resolved, &user_agent())
            .map_err(ConnectError::InvalidConnection)?;
        Ok(Route {
            provider: Arc::new(provider),
            capabilities: Some(Arc::new(ConnectionCapabilities(connection.clone()))),
            connection: Some(connection.clone()),
            model: model.map_err(ConnectError::InvalidModel)?,
            configured_model,
            source: CredentialSource::Configured,
            uses_tls,
        })
    }

    async fn codex_route(
        &self,
        requested: Option<&OsStr>,
        endpoints: SubscriptionEndpoints,
        lookup: &dyn Fn(&str) -> Option<String>,
        refreshes: Option<Arc<DetachedRefreshes>>,
        cancel: &CancellationToken,
    ) -> Result<Route, ConnectError> {
        let model = select_model(requested, |model| {
            self.settings.selected_codex_model(model, lookup)
        })?
        .map_err(ConnectError::InvalidModel)?;
        let configured_model = self.settings.selected_codex_model(None, lookup).ok();
        let uses_tls = uses_tls(&endpoints.codex.responses);
        let subscription = codex_subscription(
            self.paths.as_ref(),
            &user_agent(),
            endpoints,
            refreshes,
            cancel,
        )
        .await?;
        Ok(Route {
            provider: Arc::new(subscription.provider),
            capabilities: Some(Arc::new(subscription.capabilities)),
            connection: None,
            model,
            configured_model,
            source: CredentialSource::Codex,
            uses_tls,
        })
    }

    fn project_context(
        &self,
        command_line: &[ContextLimitOverride],
    ) -> Option<(Arc<HostProjectContext>, ProjectContext)> {
        if !self.settings.context_enabled() {
            return None;
        }
        let mut limits = self.settings.context_limits();
        limits.apply_command_line(command_line);
        let limits = InstructionLimits::from_limits(&limits);
        let home = env::var_os("HOME");
        let snapshot = gather_project_context(
            &self.workspace_root,
            ProfileLocation {
                home: home.as_deref(),
                config_directory: self.paths.as_ref().map(|paths| paths.config.as_path()),
            },
            limits,
        );
        Some((
            Arc::new(HostProjectContext::new(self.workspace_root.clone(), limits)),
            snapshot,
        ))
    }
}

struct ConnectionCapabilities(ProviderDefinition);

impl CapabilityResolver for ConnectionCapabilities {
    fn resolve<'a>(
        &'a self,
        model: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup> {
        let context_window = self.0.capabilities(model).context_window;
        Box::pin(async move {
            CapabilityLookup::Resolved(ModelCapabilities {
                context_window,
                ..ModelCapabilities::default()
            })
        })
    }
}

fn select_model(
    requested: Option<&OsStr>,
    select: impl FnOnce(Option<&str>) -> Result<String, SelectionError>,
) -> Result<Result<String, Vec<u8>>, SelectionError> {
    match requested {
        Some(requested) if requested.to_str().is_none() => Ok(Err(requested.as_bytes().to_vec())),
        requested => select(requested.and_then(OsStr::to_str)).map(Ok),
    }
}

fn output_tokens(connection: Option<&ProviderDefinition>, model: &str) -> Option<u32> {
    connection.and_then(|connection| request_output_tokens(connection.capabilities(model)))
}

fn uses_tls(url: &str) -> bool {
    url.get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

impl AgentSetup {
    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn source(&self) -> CredentialSource {
        self.source
    }

    pub fn provider(&self) -> ProviderId {
        self.connection
            .as_ref()
            .map_or(ProviderId::Codex, |connection| {
                ProviderId::Configured(connection.id().to_owned())
            })
    }

    pub fn provider_binding(&self) -> Option<[u8; 32]> {
        self.connection
            .as_ref()
            .map(ProviderDefinition::binding_identity)
    }

    pub fn configured_model(&self) -> &str {
        self.configured_model
            .as_deref()
            .unwrap_or(&self.config.model)
    }

    pub fn context_notices(&self) -> &[String] {
        &self.context_notices
    }

    pub(crate) fn models(&self) -> &[String] {
        self.connection
            .as_ref()
            .map_or(&[], |connection| connection.models())
    }

    pub(crate) fn approvals(&self) -> Option<&Approvals> {
        self.approvals.as_ref()
    }

    pub(crate) fn refreshes(&self) -> Option<Arc<DetachedRefreshes>> {
        self.refreshes.clone()
    }

    pub(crate) fn config(&self, model: &str) -> AgentConfig {
        AgentConfig {
            model: model.to_owned(),
            max_output_tokens: output_tokens(self.connection.as_ref(), model),
            ..self.config.clone()
        }
    }

    pub fn agent(&self) -> Agent {
        let mut agent = Agent::new(
            Arc::clone(&self.provider),
            self.tools.clone(),
            Arc::clone(&self.context),
            self.permissions.clone(),
            self.config.clone(),
        );
        if let Some(capabilities) = &self.capabilities {
            agent = agent.with_capability_resolver(Arc::clone(capabilities));
        }
        if let Some(approvals) = &self.approvals {
            agent = agent.with_approvals(approvals.clone());
        }
        match &self.project {
            Some((provider, snapshot)) => {
                agent.with_project_context(provider.clone(), snapshot.clone())
            }
            None => agent,
        }
    }
}

pub fn user_agent() -> String {
    format!("oh-fx/{}", ofx_upgrade::VERSION)
}

#[cfg(test)]
mod tests {
    use ofx_auth::ChatGptEndpoints;
    use ofx_exec::SessionSupervisor;
    use ofx_gateway::CodexEndpoints;
    use ofx_testkit::{FakeServer, Reply};

    use super::*;

    const EXPIRED_SESSION: &str = r#"{"version":1,"access_token":"eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl","refresh_token":"rt-refresh-secret-0123456789","expires_at_ms":1,"account_id":"acct_test"}"#;

    fn profile(directory: &Path, settings: &str) -> Profile {
        let paths = ProfilePaths {
            config: directory.join("config"),
            data: directory.join("data"),
            state: directory.join("state"),
            cache: directory.join("cache"),
        };
        let workspace = directory.join("workspace");
        for directory in [&paths.config, &paths.data, &workspace] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(paths.config.join("settings.json"), settings).unwrap();
        let settings = Settings::load(&paths, &workspace).unwrap();
        Profile::new(workspace, Some(paths), settings).unwrap()
    }

    #[tokio::test]
    async fn non_utf8_codex_models_fail_before_the_login_is_refreshed() {
        let directory = tempfile::tempdir().unwrap();
        let profile = profile(
            directory.path(),
            r#"{"provider":"codex","models":{"codex":"gpt-6.1-sol"}}"#,
        );
        let session = directory.path().join("data/chatgpt-auth.json");
        fs::write(&session, EXPIRED_SESSION).unwrap();
        let auth = FakeServer::start([Reply::status(500, "unexpected refresh")]);
        let base_url = auth.base_url();
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        let failure = profile
            .connect(
                Launch {
                    model: Some(OsStr::from_bytes(b" m\xff ")),
                    permission_mode: PermissionMode::Auto,
                    system_prompt: None,
                    reasoning_effort: None,
                    fast_mode: false,
                    context_limits: &[],
                    command_timeout: None,
                    executions: &executions,
                    endpoints: SubscriptionEndpoints {
                        chatgpt: ChatGptEndpoints {
                            issuer: base_url.clone(),
                            token_url: format!("{base_url}/oauth/token"),
                            callback_ports: vec![0],
                        },
                        codex: CodexEndpoints {
                            responses: format!("{base_url}/backend-api/codex/responses"),
                        },
                        ..SubscriptionEndpoints::default()
                    },
                },
                &CancellationToken::new(),
            )
            .await
            .err()
            .expect("an invalid model");
        assert!(
            matches!(&failure, ConnectError::InvalidModel(model) if model == b" m\xff "),
            "{failure:?}"
        );
        assert!(auth.requests().is_empty());
        assert_eq!(fs::read_to_string(session).unwrap(), EXPIRED_SESSION);
    }

    #[tokio::test]
    async fn configured_connections_resolve_the_context_window_of_their_models() {
        let directory = tempfile::tempdir().unwrap();
        let profile = profile(
            directory.path(),
            r#"{"provider":"local","providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://127.0.0.1:9/v1","auth":{"type":"none"},"model_metadata":{"sized":{"context_window":128000,"max_output_tokens":16000}}}}}"#,
        );
        let connection = profile.settings().selected_connection(&|_| None).unwrap();
        let resolver = ConnectionCapabilities(connection.clone());
        let cancel = CancellationToken::new();
        assert_eq!(
            resolver.resolve("sized", &cancel).await,
            CapabilityLookup::Resolved(ModelCapabilities {
                context_window: Some(128_000),
                ..ModelCapabilities::default()
            })
        );
        assert_eq!(
            resolver.resolve("unlisted", &cancel).await,
            CapabilityLookup::Resolved(ModelCapabilities::default())
        );
    }

    #[test]
    fn only_https_endpoints_warm_the_tls_roots() {
        assert!(uses_tls("https://gateway.example/v1/chat/completions"));
        assert!(uses_tls("HTTPS://gateway.example/v1"));
        assert!(!uses_tls("http://127.0.0.1:8080/v1/chat/completions"));
        assert!(!uses_tls("https:"));
    }

    #[test]
    fn credential_sources_name_themselves_and_the_codex_relogin() {
        assert_eq!(CredentialSource::Configured.label(), "configured provider");
        assert_eq!(CredentialSource::Codex.label(), "Codex subscription");
        assert_eq!(
            CredentialSource::Codex.relogin(),
            Some("Run oh-fx login codex to sign in again.")
        );
        assert_eq!(CredentialSource::Configured.relogin(), None);
    }
}
