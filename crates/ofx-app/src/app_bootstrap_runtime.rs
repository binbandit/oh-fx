use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ofx_agent::{
    Agent, AgentConfig, ChildStore, ProjectContext, QuestionRequests, Questions, RuntimeContext,
    SkillContextProvider,
};
use ofx_auth::{CHATGPT_RELOGIN_MESSAGE, CHATGPT_SOURCE_LABEL};
use ofx_config::{
    ConfigDiagnostic, ConnectionError, ContextLimitName, ContextLimitOverride, ContextLimits,
    ProfilePaths, ProviderDefinition, ProviderId, SelectionError, Settings, SettingsError,
    request_output_tokens,
};
use ofx_contract::{
    ActiveMode, ApprovalAnswer, CallDescription, CapabilityResolver, DynamicTools,
    LiveAdditionalRoots, LivePermissionMode, ModelControls, ModelProvider, PermissionMode,
    QuestionAsker, ReasoningEffort, RequestId, ReviewTransport, StatuslineToggles, Tool,
    is_provider_search_alias, parse_tool_args_object, provider_search_description,
};
use ofx_exec::ManagedExecutions;
use ofx_gateway::{
    CODEX_TITLE_MODEL, ChatCompletionsProvider, ChatCompletionsReviewTransport,
    CodexReviewTransport,
};
use ofx_http::ClientError;
use ofx_mcp::{ConnectOptions, McpRuntime, ProfileStoreError, SchemaLimits};
use ofx_permissions::{DEFAULT_REVIEW_TIMEOUT, PermissionPolicy, Reviewer};
use ofx_session::RouteCredential;
use ofx_tools::WebFetchProgress;
use ofx_workspace::{ChangeTracker, WorkspaceAccess, WorkspaceAccessError};
use tokio_util::sync::CancellationToken;

use crate::app_agent_runtime::Emit;
use crate::app_mcp_runtime::{McpHost, McpSources};
use crate::app_permission_runtime::PermissionRuntime;
use crate::app_subagent_runtime::{ChildFactory, Delegation, ParentCatalog};
use crate::app_workspace_runtime::WorkspaceRuntime;
use crate::approval_queue::ApprovalQueue;
use crate::codex_provider::{
    CodexUnavailable, DetachedRefreshes, SubscriptionEndpoints, SubscriptionLogin,
    SubscriptionProvider, codex_subscription,
};
use crate::context::{
    GATEWAY_SYSTEM_PROMPT, HostProjectContext, HostRuntimeContext, InstructionLimits,
    ProfileLocation, gather_project_context,
};
use crate::model_cache_runtime::{ModelSource, model_controls};
use crate::output_contracts::StatusSnapshot;
use crate::skills::HostSkills;
use crate::tool_set::{self, ToolHooks};

mod provider_runtime;

pub(crate) use provider_runtime::{provider_label, provider_names};

const CONFIGURED_SOURCE_LABEL: &str = "configured provider";
const CONFIGURED_SOURCE_REPAIR: &str = "Check the configured provider auth environment variable.";

#[derive(Clone)]
pub struct Profile {
    workspace_root: PathBuf,
    home: Option<OsString>,
    paths: Option<ProfilePaths>,
    settings: Settings,
    access: WorkspaceAccess,
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
    #[error("{0}")]
    Mcp(#[from] ProfileStoreError),
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
    pub mode: Option<ActiveMode>,
}

pub struct AgentSetup {
    provider: Arc<dyn ModelProvider>,
    login: Login,
    title_model: Option<&'static str>,
    session_titles: bool,
    prompt_history: bool,
    configured_model: Option<String>,
    models: ModelSource,
    connection: Option<ProviderDefinition>,
    source: CredentialSource,
    account_id: Option<String>,
    subscription: Option<Arc<SubscriptionLogin>>,
    tools: Vec<Arc<dyn Tool>>,
    delegation: Delegation,
    mcp: Option<Arc<McpRuntime>>,
    context: Arc<dyn RuntimeContext>,
    permission_mode: LivePermissionMode,
    workspace_root: PathBuf,
    permissions: Arc<PermissionPolicy>,
    preferences: Option<ProfilePaths>,
    workspace: WorkspaceRuntime,
    yolo_acknowledged: bool,
    approvals: Option<Arc<ApprovalQueue>>,
    change_tracker: Option<ChangeTracker>,
    questions: Option<Questions>,
    question_requests: Option<QuestionRequests>,
    statusline: StatuslineToggles,
    refreshes: Option<Arc<DetachedRefreshes>>,
    project: Option<(Arc<HostProjectContext>, ProjectContext)>,
    skills: Arc<HostSkills>,
    context_notices: Vec<String>,
    mode: Option<ActiveMode>,
    switchboard: Option<Switchboard>,
    config: AgentConfig,
}

struct Switchboard {
    profile: Profile,
    endpoints: SubscriptionEndpoints,
}

pub(crate) struct Route {
    provider: Arc<dyn ModelProvider>,
    reviewer: Arc<dyn ReviewTransport>,
    title_model: Option<&'static str>,
    models: ModelSource,
    connection: Option<ProviderDefinition>,
    model: String,
    configured_model: Option<String>,
    source: CredentialSource,
    account_id: Option<String>,
    subscription: Option<Arc<SubscriptionLogin>>,
    uses_tls: bool,
    login: Login,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Login {
    Ready,
    Missing,
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
        mut settings: Settings,
    ) -> Result<Self, ProfileError> {
        let access = WorkspaceAccess::new(&workspace_root, settings.additional_directories())
            .unwrap_or_else(|_| {
                settings.reject_additional_directories();
                WorkspaceAccess::primary_only(&workspace_root)
            });
        if settings.profile_is_unusable() {
            return Err(ProfileError::Unusable(settings.diagnostics().to_vec()));
        }
        Ok(Self {
            workspace_root,
            home,
            paths,
            settings,
            access,
        })
    }

    pub fn apply_launch(
        &mut self,
        additional_directories: &[OsString],
        saved_directories_suppressed: bool,
    ) -> Result<(), WorkspaceAccessError> {
        self.access = self
            .access
            .apply_launch(additional_directories, saved_directories_suppressed)?;
        Ok(())
    }

    fn additional_roots(&self) -> Vec<PathBuf> {
        self.access.active_roots().map(Path::to_path_buf).collect()
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
        let switchboard = interactive.then(|| self.switchboard(launch.endpoints.clone()));
        let route = self
            .launch_route(&launch, refreshes.clone(), interactive, cancel)
            .await?;
        let mut limits = self.settings.context_limits();
        limits.apply_command_line(launch.context_limits);
        let skills = self.load_skills(&limits, interactive);
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
        let additional_roots = LiveAdditionalRoots::from(self.additional_roots());
        let change_tracker = interactive.then(ChangeTracker::default);
        let (questions, question_requests) = interactive.then(Questions::new).unzip();
        let tools = tool_set::ask_tools(
            &self.workspace_root,
            launch.executions,
            launch.command_timeout,
            &permission_mode,
            skills.tool(),
            skills.search(),
            ToolHooks {
                questions: questions
                    .clone()
                    .map(|questions| Arc::new(questions) as Arc<dyn QuestionAsker>),
                web_fetch_progress: launch.web_fetch_progress,
                change_tracker: change_tracker.as_ref(),
                additional_roots: additional_roots.clone(),
            },
        );
        let permissions =
            self.reviewed_policy(&permission_mode, &route.reviewer, additional_roots.clone());
        let approvals = interactive.then(ApprovalQueue::shared);
        let mcp = self.mcp_runtime(&tools, &limits, interactive)?;
        let children = ChildFactory {
            route: Mutex::new(route.children()),
            executions: launch.executions.clone(),
            command_timeout: launch.command_timeout,
            parent_permissions: Arc::clone(&permissions),
            approvals: approvals.clone(),
            project: project.clone(),
            skills: Arc::clone(&skills),
            mcp: ParentCatalog::shared(mcp.clone().map(|mcp| mcp as Arc<dyn DynamicTools>)),
            workspace_root: self.workspace_root.clone(),
            additional_roots: additional_roots.clone(),
            permission_mode: permission_mode.clone(),
            parent: Mutex::new(config.clone()),
            mode: launch.mode,
        };
        Ok(AgentSetup {
            provider: route.provider,
            title_model: route.title_model,
            session_titles: self.settings.session_titles_enabled(),
            prompt_history: self.settings.prompt_history_enabled(),
            configured_model: route.configured_model,
            models: route.models,
            login: route.login,
            connection: route.connection,
            source: route.source,
            account_id: route.account_id,
            subscription: route.subscription,
            tools,
            delegation: Delegation::new(children),
            mcp,
            context: self.runtime_context(&permission_mode, interactive, additional_roots.clone()),
            permissions,
            permission_mode,
            preferences: self.paths.clone(),
            workspace: self.workspace_runtime(additional_roots),
            yolo_acknowledged: self.settings.yolo_acknowledged(),
            workspace_root: self.workspace_root.clone(),
            approvals,
            change_tracker,
            questions,
            question_requests,
            statusline: self.settings.statusline(),
            refreshes,
            project,
            skills,
            context_notices,
            mode: launch.mode,
            switchboard,
            config,
        })
    }

    fn workspace_runtime(&self, additional_roots: LiveAdditionalRoots) -> WorkspaceRuntime {
        WorkspaceRuntime::new(
            self.access.clone(),
            additional_roots,
            self.paths.clone().filter(|_| self.home.is_some()),
        )
    }

    fn runtime_context(
        &self,
        permission_mode: &LivePermissionMode,
        interactive: bool,
        additional_roots: LiveAdditionalRoots,
    ) -> Arc<HostRuntimeContext> {
        Arc::new(
            HostRuntimeContext::new(
                self.workspace_root.clone(),
                permission_mode.clone(),
                interactive,
            )
            .with_additional_roots(additional_roots),
        )
    }

    fn load_skills(&self, limits: &ContextLimits, interactive: bool) -> Arc<HostSkills> {
        Arc::new(HostSkills::load(
            &self.workspace_root,
            self.home.as_deref(),
            self.paths.as_ref(),
            &self.settings,
            limits,
            interactive,
        ))
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

    fn reviewed_policy(
        &self,
        permission_mode: &LivePermissionMode,
        reviewer: &Arc<dyn ReviewTransport>,
        additional_roots: LiveAdditionalRoots,
    ) -> Arc<PermissionPolicy> {
        Arc::new(
            PermissionPolicy::new(permission_mode.clone(), self.workspace_root.clone())
                .with_additional_roots(additional_roots)
                .with_reviewer(Reviewer::new(Arc::clone(reviewer), DEFAULT_REVIEW_TIMEOUT)),
        )
    }

    async fn launch_route(
        &self,
        launch: &Launch<'_>,
        refreshes: Option<Arc<DetachedRefreshes>>,
        interactive: bool,
        cancel: &CancellationToken,
    ) -> Result<Route, ConnectError> {
        let endpoints = launch.endpoints.clone();
        let route = match self.route(launch.model, endpoints, refreshes, cancel).await {
            Err(ConnectError::Codex(CodexUnavailable::MissingLogin)) if interactive => {
                self.signed_out_route(launch.model)
            }
            route => route,
        }?;
        if route.uses_tls {
            ofx_http::warm_tls_roots();
        }
        Ok(route)
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
        connection_route(connection, model, configured_model)
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
        self.subscription_route(model, configured_model, endpoints, refreshes, cancel)
            .await
    }

    fn signed_out_route(&self, requested: Option<&OsStr>) -> Result<Route, ConnectError> {
        let lookup = |name: &str| env::var(name).ok();
        let model = select_model(requested, |model| {
            self.settings.selected_codex_model(model, &lookup)
        })?
        .map_err(ConnectError::InvalidModel)?;
        Ok(Route::signed_out(
            model,
            self.settings.selected_codex_model(None, &lookup).ok(),
        ))
    }

    async fn subscription_route(
        &self,
        model: String,
        configured_model: Option<String>,
        endpoints: SubscriptionEndpoints,
        refreshes: Option<Arc<DetachedRefreshes>>,
        cancel: &CancellationToken,
    ) -> Result<Route, ConnectError> {
        let uses_tls = uses_tls(&endpoints.codex.responses);
        let subscription = codex_subscription(
            self.paths.as_ref(),
            &user_agent(),
            endpoints,
            refreshes,
            cancel,
        )
        .await?;
        let provider: Arc<dyn ModelProvider> = Arc::new(SubscriptionProvider::new(
            subscription.provider,
            Arc::clone(&subscription.login),
        ));
        Ok(Route {
            reviewer: Arc::new(CodexReviewTransport::new(Arc::clone(&provider))),
            title_model: Some(CODEX_TITLE_MODEL),
            provider,
            models: ModelSource::Codex(Arc::new(subscription.capabilities)),
            connection: None,
            model,
            configured_model,
            source: CredentialSource::Codex,
            account_id: Some(subscription.account_id),
            subscription: Some(subscription.login),
            uses_tls,
            login: Login::Ready,
        })
    }

    fn mcp_runtime(
        &self,
        tools: &[Arc<dyn Tool>],
        limits: &ContextLimits,
        interactive: bool,
    ) -> Result<Option<Arc<McpRuntime>>, ProfileStoreError> {
        let sources = McpSources::new(self.paths.clone(), self.workspace_root.clone());
        let load = sources.load_with(&self.settings)?;
        if !interactive && load.configs.is_empty() && load.workspace_diagnostics.is_empty() {
            return Ok(None);
        }
        let options = ConnectOptions {
            client_version: ofx_upgrade::VERSION.to_owned(),
            user_agent: user_agent(),
        };
        let reserved = tools.iter().map(|tool| tool.spec().name.clone()).collect();
        let limits = SchemaLimits {
            server_instructions: limits.get(ContextLimitName::McpServerInstructionsBytes),
            selected_schema: limits.get(ContextLimitName::McpSelectedSchemaBytes),
        };
        Ok(Some(Arc::new(McpRuntime::new(
            load, options, reserved, limits,
        ))))
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

fn select_model(
    requested: Option<&OsStr>,
    select: impl FnOnce(Option<&str>) -> Result<String, SelectionError>,
) -> Result<Result<String, Vec<u8>>, SelectionError> {
    match requested {
        Some(requested) if requested.to_str().is_none() => Ok(Err(requested.as_bytes().to_vec())),
        requested => select(requested.and_then(OsStr::to_str)).map(Ok),
    }
}

fn connection_route(
    connection: &ProviderDefinition,
    model: Result<String, Vec<u8>>,
    configured_model: Option<String>,
) -> Result<Route, ConnectError> {
    let lookup = |name: &str| env::var(name).ok();
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
        models: ModelSource::Connection(definition),
        connection: Some(connection.clone()),
        model: model.map_err(ConnectError::InvalidModel)?,
        configured_model,
        source: CredentialSource::Configured,
        account_id: None,
        subscription: None,
        uses_tls,
        login: Login::Ready,
    })
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

    pub(crate) fn login(&self) -> Login {
        self.login
    }

    pub fn source(&self) -> CredentialSource {
        self.source
    }

    pub fn route_credential(&self) -> RouteCredential {
        match self.source {
            CredentialSource::Configured => RouteCredential::configured(),
            CredentialSource::Codex => RouteCredential::chatgpt_subscription(
                self.account_id.as_deref().unwrap_or_default(),
            ),
        }
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

    pub fn mcp(&self) -> Option<&Arc<McpRuntime>> {
        self.mcp.as_ref()
    }

    pub(crate) fn fast_mode(&self) -> bool {
        self.config.fast_mode
    }

    pub(crate) fn session_titles_enabled(&self) -> bool {
        self.session_titles
    }

    pub(crate) fn set_session_titles(&mut self, enabled: bool) {
        self.session_titles = enabled;
    }

    pub(crate) fn prompt_history_enabled(&self) -> bool {
        self.prompt_history
    }

    pub(crate) fn title_model(&self) -> Option<&'static str> {
        self.title_model
    }

    pub(crate) fn model_provider(&self) -> Arc<dyn ModelProvider> {
        Arc::clone(&self.provider)
    }

    pub(crate) fn reasoning_effort(&self) -> ReasoningEffort {
        self.config
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse)
            .unwrap_or(ReasoningEffort::Auto)
    }

    pub(crate) fn status<'a>(
        &'a self,
        model: &'a str,
        history_turns: usize,
        ultrafast_requested: bool,
    ) -> StatusSnapshot<'a> {
        StatusSnapshot {
            model,
            connection: self.connection.as_ref(),
            source: self.source,
            permission_mode: self.permission_mode.get(),
            workspace_root: &self.workspace_root,
            history_turns,
            session_permission_grants: self.permissions.session_grant_count(),
            agent_step_limit: self.config.step_limit,
            ultrafast_requested,
        }
    }

    pub(crate) fn models_source(&self) -> ModelSource {
        self.models.clone()
    }

    pub(crate) fn model_controls(&self) -> ModelControls {
        model_controls(
            self.models.cached().as_ref(),
            self.model(),
            &self.reasoning_effort(),
            self.fast_mode(),
        )
    }

    pub(crate) fn approvals(&self) -> Option<&Arc<ApprovalQueue>> {
        self.approvals.as_ref()
    }

    pub(crate) fn permission_mode(&self) -> PermissionMode {
        self.permission_mode.get()
    }

    pub(crate) fn step_limit(&self) -> u64 {
        self.config.step_limit
    }

    pub(crate) fn answer_approval(&self, id: RequestId, answer: ApprovalAnswer) {
        if let Some(approvals) = &self.approvals {
            approvals.resolve(id, answer);
        }
    }

    pub(crate) fn end_turn_approvals(&self) {
        if let Some(approvals) = &self.approvals {
            approvals.turn_finished();
        }
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .chain([&self.delegation.tool])
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

    pub(crate) fn statusline(&self) -> StatuslineToggles {
        self.statusline
    }

    pub(crate) fn preferences(&self) -> Option<&ProfilePaths> {
        self.preferences.as_ref()
    }

    pub(crate) fn workspace(&self) -> &WorkspaceRuntime {
        &self.workspace
    }

    pub(crate) fn workspace_mut(&mut self) -> &mut WorkspaceRuntime {
        &mut self.workspace
    }

    pub(crate) fn mcp_host(&self, emit: Emit) -> Option<McpHost> {
        let runtime = Arc::clone(self.mcp.as_ref()?);
        let sources = McpSources::new(self.preferences.clone(), self.workspace_root.clone());
        Some(McpHost::new(runtime, sources, emit))
    }

    #[cfg(test)]
    pub(crate) fn without_preferences(mut self) -> Self {
        self.preferences = None;
        self
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

    pub(crate) fn delegate_as(&self, config: &AgentConfig) {
        self.delegation.children.follow(config);
    }

    pub(crate) fn forget_children(&self) {
        self.delegation.host.clear();
    }

    pub(crate) fn describe_saved_call(
        &self,
        tool_name: &str,
        arguments: &str,
    ) -> Option<CallDescription> {
        if is_provider_search_alias(tool_name) {
            return parse_tool_args_object(arguments)
                .is_ok()
                .then(|| provider_search_description(arguments));
        }
        self.tools
            .iter()
            .chain([&self.delegation.tool])
            .find(|tool| tool.spec().name == tool_name)?
            .describe_saved(arguments)
    }

    pub fn bind_children(&self, store: Option<Arc<dyn ChildStore>>) {
        self.delegation.host.bind(store);
    }

    pub fn agent(&self, delegation: bool) -> Agent {
        let tools = if delegation {
            tool_set::with_subagent(&self.tools, &self.delegation.tool)
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
        .with_skills(Arc::clone(&self.skills) as Arc<dyn SkillContextProvider>)
        .with_capability_resolver(Arc::new(self.models.clone()) as Arc<dyn CapabilityResolver>);
        if let Some(mcp) = &self.mcp {
            agent = agent.with_dynamic_tools(Arc::clone(mcp) as _);
        }
        if let Some(approvals) = &self.approvals {
            agent = agent.with_approvals(approvals.approvals().clone());
        }
        if let Some(mode) = self.mode {
            agent = agent.with_mode(mode);
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
    use ofx_contract::{
        ActiveMode, CapabilityLookup, ModeRegistry, ModeSpec, ModelCapabilities, ToolPolicy,
    };
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
                    mode: None,
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
                    mode: None,
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
        let resolver = ModelSource::Connection(Arc::new(connection.clone()));
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

    static INSPECTING: [ModeSpec; 1] = [ModeSpec {
        id: "inspect",
        name: "Inspect",
        description: "",
        permission_mode: PermissionMode::Ask,
        tool_policy: ToolPolicy::ReadOnly,
        tool_policy_denial_message: None,
    }];

    static INSPECTION: ModeRegistry = ModeRegistry {
        default_mode_id: "inspect",
        modes: &INSPECTING,
    };

    fn offered(request: &ofx_testkit::RecordedRequest) -> Vec<String> {
        request.json()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn children_work_under_the_launch_modes_tool_projection() {
        let server = FakeServer::start([
            Reply::sse(&ofx_testkit::chat_tool_call_events(
                "call_1",
                "subagent",
                r#"{"request":{"action":"run","task":"look around"}}"#,
            )),
            Reply::sse(&ofx_testkit::chat_text_events(&["child report"])),
            Reply::sse(&ofx_testkit::chat_text_events(&["parent done"])),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let profile = profile(
            &root,
            &format!(
                r#"{{"provider":"local","providers":{{"local":{{"protocol":"openai-chat-completions","base_url":"{}","auth":{{"type":"none"}},"models":["model-a"]}}}}}}"#,
                server.base_url()
            ),
        );
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        let setup = profile
            .connect(
                Launch {
                    model: None,
                    permission_mode: PermissionMode::Ask,
                    system_prompt: None,
                    reasoning_effort: None,
                    fast_mode: None,
                    context_limits: &[],
                    command_timeout: None,
                    executions: &executions,
                    web_fetch_progress: None,
                    endpoints: SubscriptionEndpoints::default(),
                    mode: Some(ActiveMode {
                        registry: &INSPECTION,
                        id: "inspect",
                        read_only_tool_names: &["read_file", "glob_files", "subagent"],
                    }),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut agent = setup.agent(true);
        let report = agent
            .run_turn("go", &mut |_| {}, &CancellationToken::new())
            .await;
        assert_eq!(report.final_text, "parent done");
        let requests = server.requests();
        assert_eq!(
            offered(&requests[0]),
            ["read_file", "glob_files", "subagent"]
        );
        assert_eq!(offered(&requests[1]), ["read_file", "glob_files"]);
    }

    fn workspace_entry(root: &Path, entry: &str) -> String {
        let workspace = serde_json::to_string(&root.join("workspace")).unwrap();
        format!(r#"{{"workspaces":{{{workspace}:{entry}}}}}"#)
    }

    #[test]
    fn saved_directories_that_cannot_be_resolved_are_dropped_with_upstreams_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let primary = serde_json::to_string(&root.join("workspace")).unwrap();
        let profile = profile(
            &root,
            &workspace_entry(
                &root,
                &format!(r#"{{"additional_directories":[{primary}]}}"#),
            ),
        );
        assert!(profile.settings().additional_directories().is_empty());
        assert_eq!(
            profile
                .settings()
                .diagnostics()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "config user: invalid_additional_directories; key=additional_directories; additional_directories must be an array of at most 16 unique absolute directory paths for the current primary workspace"
            ]
        );
        assert!(profile.additional_roots().is_empty());
    }

    #[test]
    fn launch_directories_join_the_saved_ones_or_fail_with_upstream_error_names() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let saved = root.join("saved");
        let shared = root.join("shared");
        fs::create_dir_all(&saved).unwrap();
        fs::create_dir_all(&shared).unwrap();
        let saved_json = serde_json::to_string(&saved).unwrap();
        let mut profile = profile(
            &root,
            &workspace_entry(
                &root,
                &format!(r#"{{"additional_directories":[{saved_json}]}}"#),
            ),
        );
        assert_eq!(profile.additional_roots(), std::slice::from_ref(&saved));
        profile
            .apply_launch(&[OsString::from("../shared")], false)
            .unwrap();
        assert_eq!(profile.additional_roots(), [saved, shared.clone()]);
        profile
            .apply_launch(&[OsString::from("../shared")], true)
            .unwrap();
        assert_eq!(profile.additional_roots(), [shared]);
        assert_eq!(
            profile.apply_launch(&[OsString::from("../missing")], false),
            Err(WorkspaceAccessError::PathNotFound)
        );
    }

    fn system_texts(request: &ofx_testkit::RecordedRequest) -> Vec<String> {
        request.json()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "system")
            .map(|message| message["content"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn an_upgrade_relaunch_keeps_the_launchs_directory_access() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        for name in ["saved", "cli-only"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        let saved = serde_json::to_string(&root.join("saved")).unwrap();
        let settings =
            workspace_entry(&root, &format!(r#"{{"additional_directories":[{saved}]}}"#));
        let Ok(ofx_cli::Invocation::Interactive(launch)) =
            ofx_cli::parse_args(["--add-dir", "../cli-only", "--no-additional-dirs"])
        else {
            panic!("the launch is interactive");
        };
        let relaunch =
            crate::app_upgrade_runtime::Relaunch::carrying(launch.relaunch_args().to_vec());
        relaunch.request(PathBuf::from("/tmp/oh-fx-upgraded"), false);
        relaunch.hand_off("session-123");
        let mut argv = Vec::new();
        let _ = relaunch.run_with(|command| {
            argv.extend(command.get_args().map(OsStr::to_os_string));
            std::io::Error::from(std::io::ErrorKind::NotFound)
        });
        let Ok(ofx_cli::Invocation::Resume(resumed, _)) = ofx_cli::parse_args(argv) else {
            panic!("the relaunch resumes the session");
        };
        let mut launched = profile(&root, &settings);
        launched
            .apply_launch(
                launch.additional_directories(),
                launch.saved_directories_suppressed(),
            )
            .unwrap();
        let mut relaunched = profile(&root, &settings);
        relaunched
            .apply_launch(
                resumed.additional_directories(),
                resumed.saved_directories_suppressed(),
            )
            .unwrap();
        assert_eq!(launched.additional_roots(), [root.join("cli-only")]);
        assert_eq!(relaunched.additional_roots(), launched.additional_roots());
    }

    #[tokio::test]
    async fn children_share_the_launchs_additional_directories() {
        let server = FakeServer::start([
            Reply::sse(&ofx_testkit::chat_tool_call_events(
                "call_1",
                "subagent",
                r#"{"request":{"action":"run","task":"look around"}}"#,
            )),
            Reply::sse(&ofx_testkit::chat_text_events(&["child report"])),
            Reply::sse(&ofx_testkit::chat_text_events(&["parent done"])),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let shared = root.join("shared");
        fs::create_dir_all(&shared).unwrap();
        let mut profile = profile(
            &root,
            &format!(
                r#"{{"provider":"local","providers":{{"local":{{"protocol":"openai-chat-completions","base_url":"{}","auth":{{"type":"none"}},"models":["model-a"]}}}}}}"#,
                server.base_url()
            ),
        );
        profile
            .apply_launch(&[OsString::from("../shared")], false)
            .unwrap();
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        let setup = profile
            .connect(
                Launch {
                    model: None,
                    permission_mode: PermissionMode::Ask,
                    system_prompt: None,
                    reasoning_effort: None,
                    fast_mode: None,
                    context_limits: &[],
                    command_timeout: None,
                    executions: &executions,
                    web_fetch_progress: None,
                    endpoints: SubscriptionEndpoints::default(),
                    mode: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut agent = setup.agent(true);
        let report = agent
            .run_turn("go", &mut |_| {}, &CancellationToken::new())
            .await;
        assert_eq!(report.final_text, "parent done");
        let note = format!(
            "Runtime context: the following additional directories are access-authorized for this run. Relative paths still resolve from the primary workspace. These directories do not contribute AGENTS.md or other project instructions.\n- {}\n",
            shared.display()
        );
        let requests = server.requests();
        assert!(system_texts(&requests[0]).contains(&note));
        assert!(system_texts(&requests[1]).contains(&note));
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
