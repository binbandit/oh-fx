use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ofx_agent::{
    Agent, AgentConfig, Approvals, ProjectContext, QuestionRequests, Questions, RuntimeContext,
    SkillContextProvider, SubagentHost,
};
use ofx_auth::{CHATGPT_RELOGIN_MESSAGE, CHATGPT_SOURCE_LABEL};
use ofx_config::{
    ConfigDiagnostic, ConnectionError, ContextLimitOverride, ContextLimits, ProfilePaths,
    ProviderDefinition, ProviderId, SelectionError, Settings, SettingsError, request_output_tokens,
};
use ofx_contract::{
    BoxFuture, CapabilityLookup, CapabilityResolver, LivePermissionMode, ModelCapabilities,
    ModelProvider, PermissionMode, QuestionAsker, ReviewTransport, Tool,
};
use ofx_exec::ManagedExecutions;
use ofx_gateway::{
    CODEX_TITLE_MODEL, ChatCompletionsProvider, ChatCompletionsReviewTransport,
    CodexReviewTransport,
};
use ofx_http::ClientError;
use ofx_permissions::{DEFAULT_REVIEW_TIMEOUT, PermissionPolicy, Reviewer};
use ofx_tools::{SubagentTool, WebFetchProgress};
use ofx_workspace::ChangeTracker;
use tokio_util::sync::CancellationToken;

use crate::app_agent_runtime::Emit;
use crate::app_permission_runtime::PermissionRuntime;
use crate::app_subagent_runtime::ChildFactory;
use crate::codex_provider::{
    CodexUnavailable, DetachedRefreshes, SubscriptionEndpoints, codex_subscription,
};
use crate::context::{
    GATEWAY_SYSTEM_PROMPT, HostProjectContext, HostRuntimeContext, InstructionLimits,
    ProfileLocation, gather_project_context,
};
use crate::output_contracts::StatusSnapshot;
use crate::skills::HostSkills;
use crate::tool_set::{self, ToolHooks};

const CONFIGURED_SOURCE_LABEL: &str = "configured provider";
const CONFIGURED_SOURCE_REPAIR: &str = "Check the configured provider auth environment variable.";

pub struct Profile {
    workspace_root: PathBuf,
    home: Option<OsString>,
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

    pub(crate) const fn refreshable(self) -> bool {
        matches!(self, Self::Codex)
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
    pub fast_mode: Option<bool>,
    pub context_limits: &'a [ContextLimitOverride],
    pub command_timeout: Option<Duration>,
    pub executions: &'a ManagedExecutions,
    pub endpoints: SubscriptionEndpoints,
    pub web_fetch_progress: Option<WebFetchProgress>,
}

pub struct AgentSetup {
    provider: Arc<dyn ModelProvider>,
    title_model: Option<&'static str>,
    session_titles: bool,
    configured_model: Option<String>,
    capabilities: Option<Arc<dyn CapabilityResolver>>,
    connection: Option<ProviderDefinition>,
    source: CredentialSource,
    tools: Vec<Arc<dyn Tool>>,
    subagent: Arc<dyn Tool>,
    context: Arc<dyn RuntimeContext>,
    permission_mode: LivePermissionMode,
    workspace_root: PathBuf,
    permissions: Arc<PermissionPolicy>,
    preferences: Option<ProfilePaths>,
    yolo_acknowledged: bool,
    approvals: Option<Approvals>,
    change_tracker: Option<ChangeTracker>,
    questions: Option<Questions>,
    question_requests: Option<QuestionRequests>,
    refreshes: Option<Arc<DetachedRefreshes>>,
    project: Option<(Arc<HostProjectContext>, ProjectContext)>,
    skills: Arc<HostSkills>,
    context_notices: Vec<String>,
    config: AgentConfig,
}

struct Route {
    provider: Arc<dyn ModelProvider>,
    reviewer: Arc<dyn ReviewTransport>,
    title_model: Option<&'static str>,
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
        Self::new(workspace_root, env::var_os("HOME"), paths, settings)
    }

    pub(crate) fn new(
        workspace_root: PathBuf,
        home: Option<OsString>,
        paths: Option<ProfilePaths>,
        settings: Settings,
    ) -> Result<Self, ProfileError> {
        if settings.profile_is_unusable() {
            return Err(ProfileError::Unusable(settings.diagnostics().to_vec()));
        }
        Ok(Self {
            workspace_root,
            home,
            paths,
            settings,
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn paths(&self) -> Option<&ProfilePaths> {
        self.paths.as_ref()
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn data_dir(&self) -> Option<&Path> {
        self.paths.as_ref().map(|paths| paths.data.as_path())
    }

    pub(crate) fn cache_dir(&self) -> Option<&Path> {
        self.paths.as_ref().map(|paths| paths.cache.as_path())
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
        let mut limits = self.settings.context_limits();
        limits.apply_command_line(launch.context_limits);
        let skills = Arc::new(HostSkills::load(
            &self.workspace_root,
            self.home.as_deref(),
            self.paths.as_ref(),
            &self.settings,
            &limits,
        ));
        let mut project = self.project_context(&limits);
        let context_notices = project
            .as_mut()
            .map(|(_, snapshot)| mem::take(&mut snapshot.notices))
            .unwrap_or_default();
        let config = self.agent_config(
            &route,
            launch.system_prompt,
            launch.reasoning_effort,
            launch.fast_mode,
        );
        let permission_mode = LivePermissionMode::from(launch.permission_mode);
        let change_tracker = interactive.then(ChangeTracker::default);
        let (questions, question_requests) = interactive.then(Questions::new).unzip();
        let tools_with = |hooks| {
            tool_set::ask_tools(
                &self.workspace_root,
                launch.executions,
                launch.command_timeout,
                &permission_mode,
                skills.tool(),
                hooks,
            )
        };
        let tools = tools_with(ToolHooks {
            questions: questions
                .clone()
                .map(|questions| Arc::new(questions) as Arc<dyn QuestionAsker>),
            web_fetch_progress: launch.web_fetch_progress,
            change_tracker: change_tracker.as_ref(),
        });
        let children = ChildFactory {
            provider: Arc::clone(&route.provider),
            tools: tools_with(ToolHooks::default()),
            capabilities: route.capabilities.clone(),
            connection: route.connection.clone(),
            reviewer: Arc::clone(&route.reviewer),
            project: project.clone(),
            skills: Arc::clone(&skills),
            workspace_root: self.workspace_root.clone(),
            permission_mode: permission_mode.clone(),
            config: config.clone(),
        };
        Ok(AgentSetup {
            provider: route.provider,
            title_model: route.title_model,
            session_titles: self.settings.session_titles_enabled(),
            configured_model: route.configured_model,
            capabilities: route.capabilities,
            connection: route.connection,
            source: route.source,
            tools,
            subagent: Arc::new(SubagentTool::new(Arc::new(SubagentHost::new(Arc::new(
                children,
            ))))),
            context: Arc::new(HostRuntimeContext::new(
                self.workspace_root.clone(),
                permission_mode.clone(),
                interactive,
            )),
            permissions: Arc::new(
                PermissionPolicy::new(permission_mode.clone(), self.workspace_root.clone())
                    .with_reviewer(Reviewer::new(route.reviewer, DEFAULT_REVIEW_TIMEOUT)),
            ),
            permission_mode,
            preferences: self.paths.clone(),
            yolo_acknowledged: self.settings.yolo_acknowledged(),
            workspace_root: self.workspace_root.clone(),
            approvals: interactive.then(Approvals::default),
            change_tracker,
            questions,
            question_requests,
            refreshes,
            project,
            skills,
            context_notices,
            config,
        })
    }

    fn agent_config(
        &self,
        route: &Route,
        system_prompt: Option<String>,
        reasoning_effort: Option<String>,
        fast_mode: Option<bool>,
    ) -> AgentConfig {
        let lookup = |name: &str| env::var(name).ok();
        AgentConfig {
            system_prompt: system_prompt.unwrap_or_else(|| GATEWAY_SYSTEM_PROMPT.to_owned()),
            max_output_tokens: output_tokens(route.connection.as_ref(), &route.model),
            step_limit: self.settings.max_agent_steps(&lookup),
            model: route.model.clone(),
            reasoning_effort,
            fast_mode: fast_mode.unwrap_or_else(|| {
                self.settings.fast_mode_for(
                    &connection_provider(route.connection.as_ref()),
                    &route.model,
                )
            }),
            auto_compact_percent: self.settings.auto_compact_percent(&lookup),
        }
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
        let provider: Arc<dyn ModelProvider> = Arc::new(
            ChatCompletionsProvider::new(resolved, &user_agent())
                .map_err(ConnectError::InvalidConnection)?,
        );
        let definition = Arc::new(connection.clone());
        let limits = Arc::clone(&definition);
        let reviewer = ChatCompletionsReviewTransport::new(
            Arc::clone(&provider),
            connection.reviewer_model().map(str::to_owned),
            move |model| limits.capabilities(model).max_output_tokens,
        );
        Ok(Route {
            provider,
            reviewer: Arc::new(reviewer),
            title_model: None,
            capabilities: Some(Arc::new(ConnectionCapabilities(definition))),
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
        let provider: Arc<dyn ModelProvider> = Arc::new(subscription.provider);
        Ok(Route {
            reviewer: Arc::new(CodexReviewTransport::new(Arc::clone(&provider))),
            title_model: Some(CODEX_TITLE_MODEL),
            provider,
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
        limits: &ContextLimits,
    ) -> Option<(Arc<HostProjectContext>, ProjectContext)> {
        if !self.settings.context_enabled() {
            return None;
        }
        let limits = InstructionLimits::from_limits(limits);
        let snapshot = gather_project_context(
            &self.workspace_root,
            ProfileLocation {
                home: self.home.as_deref(),
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

struct ConnectionCapabilities(Arc<ProviderDefinition>);

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

fn connection_provider(connection: Option<&ProviderDefinition>) -> ProviderId {
    connection.map_or(ProviderId::Codex, |connection| {
        ProviderId::Configured(connection.id().to_owned())
    })
}

pub(crate) fn output_tokens(connection: Option<&ProviderDefinition>, model: &str) -> Option<u32> {
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
        connection_provider(self.connection.as_ref())
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

    pub(crate) fn fast_mode(&self) -> bool {
        self.config.fast_mode
    }

    pub(crate) fn session_titles_enabled(&self) -> bool {
        self.session_titles
    }

    pub(crate) fn title_model(&self) -> Option<&'static str> {
        self.title_model
    }

    pub(crate) fn model_provider(&self) -> Arc<dyn ModelProvider> {
        Arc::clone(&self.provider)
    }

    pub(crate) fn status<'a>(&'a self, model: &'a str, history_turns: usize) -> StatusSnapshot<'a> {
        StatusSnapshot {
            model,
            connection: self.connection.as_ref(),
            source: self.source,
            permission_mode: self.permission_mode.get(),
            workspace_root: &self.workspace_root,
            history_turns,
            session_permission_grants: self.permissions.session_grant_count(),
            agent_step_limit: self.config.step_limit,
        }
    }

    pub(crate) async fn supports_fast_mode(&self, model: &str) -> bool {
        let Some(resolver) = &self.capabilities else {
            return false;
        };
        matches!(
            resolver.resolve(model, &CancellationToken::new()).await,
            CapabilityLookup::Resolved(capabilities) if capabilities.supports_fast_mode
        )
    }

    pub(crate) fn approvals(&self) -> Option<&Approvals> {
        self.approvals.as_ref()
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|tool| tool.spec().name.clone())
            .collect()
    }

    pub(crate) fn change_tracker(&self) -> Option<&ChangeTracker> {
        self.change_tracker.as_ref()
    }

    pub(crate) fn questions(&self) -> Option<&Questions> {
        self.questions.as_ref()
    }

    pub(crate) fn take_question_requests(&mut self) -> Option<QuestionRequests> {
        self.question_requests.take()
    }

    pub(crate) fn preferences(&self) -> Option<&ProfilePaths> {
        self.preferences.as_ref()
    }

    pub(crate) fn permission_runtime(&self, emit: Emit) -> PermissionRuntime {
        PermissionRuntime::new(
            self.permission_mode.clone(),
            Arc::clone(&self.permissions),
            self.preferences.clone(),
            self.yolo_acknowledged,
            emit,
        )
    }

    pub(crate) fn refreshes(&self) -> Option<Arc<DetachedRefreshes>> {
        self.refreshes.clone()
    }

    pub(crate) fn skills(&self) -> &HostSkills {
        &self.skills
    }

    pub(crate) fn restore_reasoning(&mut self, reasoning_effort: Option<String>, fast_mode: bool) {
        self.config.reasoning_effort = reasoning_effort;
        self.config.fast_mode = fast_mode;
    }

    pub(crate) fn config(&self, model: &str) -> AgentConfig {
        AgentConfig {
            model: model.to_owned(),
            max_output_tokens: output_tokens(self.connection.as_ref(), model),
            ..self.config.clone()
        }
    }

    pub fn agent(&self, delegation: bool) -> Agent {
        let tools = if delegation {
            tool_set::with_subagent(&self.tools, &self.subagent)
        } else {
            self.tools.clone()
        };
        let mut agent = Agent::new(
            Arc::clone(&self.provider),
            tools,
            Arc::clone(&self.context),
            self.permissions.clone(),
            self.config.clone(),
        )
        .with_skills(Arc::clone(&self.skills) as Arc<dyn SkillContextProvider>);
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
        Profile::new(workspace, Some(directory.into()), Some(paths), settings).unwrap()
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
                    fast_mode: None,
                    context_limits: &[],
                    command_timeout: None,
                    executions: &executions,
                    web_fetch_progress: None,
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
    async fn a_saved_fast_choice_reaches_only_the_model_saved_with_it_unless_a_flag_decides() {
        let directory = tempfile::tempdir().unwrap();
        let profile = profile(
            directory.path(),
            r#"{"provider":"local","models":{"local":"model-a"},"fast_mode":true,"fast_mode_model_bound":true,"providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://127.0.0.1:9/v1","auth":{"type":"none"}}}}"#,
        );
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        for (model, flag, fast) in [
            (None, None, true),
            (Some("model-a"), None, true),
            (Some("model-b"), None, false),
            (Some("model-b"), Some(true), true),
            (None, Some(false), false),
        ] {
            for interactive in [false, true] {
                let launch = Launch {
                    model: model.map(OsStr::new),
                    permission_mode: PermissionMode::Auto,
                    system_prompt: None,
                    reasoning_effort: None,
                    fast_mode: flag,
                    context_limits: &[],
                    command_timeout: None,
                    executions: &executions,
                    endpoints: SubscriptionEndpoints::default(),
                    web_fetch_progress: None,
                };
                let cancel = CancellationToken::new();
                let setup = if interactive {
                    profile.connect_interactive(launch, &cancel).await
                } else {
                    profile.connect(launch, &cancel).await
                }
                .unwrap();
                assert_eq!(setup.fast_mode(), fast, "{model:?} {flag:?} {interactive}");
            }
        }
    }

    #[tokio::test]
    async fn configured_connections_resolve_the_context_window_of_their_models() {
        let directory = tempfile::tempdir().unwrap();
        let profile = profile(
            directory.path(),
            r#"{"provider":"local","providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://127.0.0.1:9/v1","auth":{"type":"none"},"model_metadata":{"sized":{"context_window":128000,"max_output_tokens":16000}}}}}"#,
        );
        let connection = profile.settings().selected_connection(&|_| None).unwrap();
        let resolver = ConnectionCapabilities(Arc::new(connection.clone()));
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
