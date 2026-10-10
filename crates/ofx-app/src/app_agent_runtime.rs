mod settings_menu;

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_agent::{
    Agent, Compaction, CompactionError, EventSink, QuestionRequests, QueuedPrompt, TurnFailure,
    TurnReport, WorkerRuntime,
};
use ofx_auth::ChatGptError;
use ofx_config::save_model_preference;
use ofx_contract::{
    BoxFuture, CompactionActivity, CompactionEnd, ModelCatalog, ModelControls, ModelOption, Notice,
    NoticeTone, ProviderError, QuestionRequest, ReasoningEffort, RecoveredTurn, ResumeRefusal,
    SessionCursor, SessionScope, SkillBinding, StatuslineItem, StatuslineToggles, TurnId,
    TurnOutcome, UiCommand, UiEvent,
};
use ofx_session::{SessionCatalog, SessionError, prompt_display_title};
use ofx_tui::Clipboard;
use ofx_workspace::ChangeTracker;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::app_bootstrap_runtime::{AgentSetup, CredentialSource, Login};
use crate::app_commands::{
    CommandEffect, ModelChange, ModelPick, Outcome, Work, change_model, handle_command, listed,
    refuse_resume_during_turn, rename_session,
};
use crate::app_mcp_runtime::McpHost;
use crate::app_permission_runtime::PermissionRuntime;
use crate::app_session_runtime::{
    Listed, NOT_CONTINUED, PageRequest, Persistence, RECOVERY_TOPIC, RestoredPreferences,
    SIGN_IN_TO_CONTINUE, SessionListing, SessionTitle,
};
use crate::app_upgrade_runtime::{ResumeHandoff, UpgradeShortcut};
use crate::app_workspace_runtime::WorkspaceRuntime;
use crate::approval_queue::ApprovalQueue;
use crate::model_cache_runtime::{ModelSource, model_controls};
use crate::native::NativeClipboard;
use crate::session_commands::{SessionFacts, SettingsAccess, handle_statusline, set_statusline};
use crate::skill_commands::{
    InstallTask, complete_install, finish_install, handle_skills, is_install_command, wait_install,
};
use crate::skills::{HostSkills, SkillInstall};
use crate::user_settings::{self, unsaved_notice};
use ofx_cli::{SLASH_REGISTRY, SlashKind};
use settings_menu::{MenuSettings, SettingsUpdate};

mod provider_switch;
use provider_switch::{PendingSignIn, SignInControl, signed_in};
mod sign_out;

pub(crate) type Emit = Arc<dyn Fn(UiEvent) + Send + Sync>;

const CONTEXT_TOPIC: &str = "context";
const MODEL_TOPIC: &str = "model";
const MODEL_PICKER_TOPIC: &str = "model picker";
const LEGACY_CONTEXT_LINE: &str = "[context]";
const LEGACY_CONTEXT_PREFIX: &str = "[context] ";

pub(crate) struct ControllerState {
    setup: AgentSetup,
    model: String,
    effort: ReasoningEffort,
    speed: Speed,
    config_pending: bool,
    pending_clear: Option<u64>,
    pending_install_inputs: VecDeque<InstallInput>,
    received_prompts: u64,
    worker: Arc<WorkerRuntime>,
    permissions: PermissionRuntime,
    context_notices: Arc<Mutex<ContextNotices>>,
    emit: Emit,
    clipboard: Arc<dyn Clipboard>,
    last_reply: Option<Arc<str>>,
    history_turns: usize,
    context_to_compact: bool,
    session_title: SessionTitle,
    statusline: StatuslineToggles,
    mcp: Option<McpHost>,
    menu_settings: MenuSettings,
    shown_controls: ModelControls,
    sign_in: Option<SignInControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Speed {
    Normal,
    Fast,
    UltraRequested,
}

enum InstallInput {
    Skills {
        rest: String,
        accepted: Option<SkillInstall>,
    },
    Clear(u64),
    Prompt(QueuedPrompt),
}

impl ControllerState {
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn permissions(&self) -> &PermissionRuntime {
        &self.permissions
    }

    pub(crate) fn workspace(&self) -> &WorkspaceRuntime {
        self.setup.workspace()
    }

    pub(crate) fn workspace_mut(&mut self) -> &mut WorkspaceRuntime {
        self.setup.workspace_mut()
    }

    pub(crate) fn effort(&self) -> &ReasoningEffort {
        &self.effort
    }

    pub(crate) fn fast_mode(&self) -> bool {
        self.speed == Speed::Fast
    }

    fn model_controls(&self) -> ModelControls {
        model_controls(
            self.setup.models_source().cached().as_ref(),
            &self.model,
            &self.effort,
            self.fast_mode(),
        )
    }

    fn sync_model_controls(&mut self) {
        let controls = self.model_controls();
        if controls != self.shown_controls {
            self.shown_controls = controls.clone();
            self.emit(UiEvent::ModelControlsChanged { controls });
        }
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        if enabled {
            self.speed = Speed::Fast;
        } else if self.speed == Speed::Fast {
            self.speed = Speed::Normal;
        }
    }

    pub(crate) fn ultrafast_requested(&self) -> bool {
        self.speed == Speed::UltraRequested
    }

    pub(crate) fn withdraw_ultrafast_request(&mut self) {
        if self.speed == Speed::UltraRequested {
            self.speed = Speed::Normal;
        }
    }

    fn restore_speed(&mut self, fast_mode: bool, ultrafast_mode: bool) {
        self.speed = if ultrafast_mode {
            Speed::UltraRequested
        } else if fast_mode {
            Speed::Fast
        } else {
            Speed::Normal
        };
    }

    pub(crate) fn save_model_preference(&self, topic: &str, effort: Option<&ReasoningEffort>) {
        let provider = self.setup.provider();
        let saved = user_settings::save(self.setup.preferences(), |paths| {
            save_model_preference(
                paths,
                &provider,
                &self.model,
                effort.map(ReasoningEffort::label),
                self.fast_mode(),
            )
        });
        if let Err(unsaved) = saved {
            self.emit(UiEvent::Notice {
                notice: unsaved_notice(topic, &unsaved),
            });
        }
    }

    pub(crate) fn status_body(&self) -> String {
        self.setup
            .status(&self.model, self.history_turns, self.ultrafast_requested())
            .render_interactive_body()
    }

    pub(crate) fn has_context_to_compact(&self) -> bool {
        self.context_to_compact
    }

    pub(crate) fn last_reply(&self) -> Option<&str> {
        self.last_reply.as_deref()
    }

    pub(crate) fn settings_access(&self) -> SettingsAccess<'_> {
        SettingsAccess {
            paths: self.setup.preferences(),
            workspace_root: self.setup.workspace_root(),
            tool_names: self.setup.tool_names(),
        }
    }

    pub(crate) fn session_facts(&self) -> SessionFacts<'_> {
        SessionFacts {
            model: &self.model,
            permission_mode: self.setup.permission_mode(),
            step_limit: self.setup.step_limit(),
        }
    }

    pub(crate) fn change_tracker(&self) -> Option<&ChangeTracker> {
        self.setup.change_tracker()
    }

    pub(crate) fn toggle_statusline(&mut self, payload: &str) {
        let access = SettingsAccess {
            paths: self.setup.preferences(),
            workspace_root: self.setup.workspace_root(),
            tool_names: Vec::new(),
        };
        for event in handle_statusline(&access, &mut self.statusline, payload) {
            self.emit(event);
        }
    }

    pub(crate) fn flip_statusline(&mut self, item: StatuslineItem) {
        let enabled = !self.statusline.enabled(item);
        let access = SettingsAccess {
            paths: self.setup.preferences(),
            workspace_root: self.setup.workspace_root(),
            tool_names: Vec::new(),
        };
        for event in set_statusline(&access, &mut self.statusline, item, enabled) {
            self.emit(event);
        }
    }

    pub(crate) fn clipboard(&self) -> &dyn Clipboard {
        &*self.clipboard
    }

    pub(crate) fn skills(&self) -> &HostSkills {
        self.setup.skills()
    }

    pub(crate) fn session_title(&self) -> &SessionTitle {
        &self.session_title
    }

    pub(crate) fn mcp(&self) -> Option<&McpHost> {
        self.mcp.as_ref()
    }

    pub(crate) fn claim_context_notice(&self, text: &str) -> bool {
        self.lock_context_notices().claim(text).is_some()
    }

    fn lock_context_notices(&self) -> MutexGuard<'_, ContextNotices> {
        self.context_notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn emit(&self, event: UiEvent) {
        (self.emit)(event);
    }

    pub(crate) fn feedback(&self) {
        crate::feedback_command::start_feedback(&self.emit);
    }

    pub(crate) fn compaction(&self, activity: CompactionActivity) {
        self.emit(UiEvent::CompactionActivity { activity });
    }

    pub(crate) fn notice(&self, tone: NoticeTone, topic: &str, body: &str) {
        self.emit(UiEvent::Notice {
            notice: Notice::new(tone, topic, body),
        });
    }

    pub(crate) fn select_model(&mut self, model: String) {
        if model != self.model {
            self.speed = Speed::Normal;
        }
        self.use_model(model);
        self.save_model_preference(MODEL_TOPIC, None);
    }

    pub(crate) fn apply_pick(
        &mut self,
        model: String,
        effort: Option<&ReasoningEffort>,
        fast_mode: bool,
    ) {
        if let Some(effort) = effort {
            effort.clone_into(&mut self.effort);
        }
        self.speed = if fast_mode {
            Speed::Fast
        } else {
            Speed::Normal
        };
        self.use_model(model);
        self.save_model_preference(MODEL_PICKER_TOPIC, effort);
    }

    fn use_model(&mut self, model: String) {
        self.model = model;
        self.emit(UiEvent::ModelSelected {
            model: self.model.clone(),
        });
    }

    fn receive_prompt(&mut self, text: String, skills: Vec<SkillBinding>, installing: bool) {
        let signed_out = self.setup.login() == Login::Missing;
        if signed_out {
            self.refuse_signed_out();
        }
        if signed_out || self.sign_in.is_some() {
            self.emit(UiEvent::PromptHeld);
        }
        let prompt = QueuedPrompt::new(self.received_prompts, text, skills);
        self.received_prompts += 1;
        if installing {
            self.pending_install_inputs
                .push_back(InstallInput::Prompt(prompt));
        } else {
            self.worker.admit(prompt);
        }
    }

    pub(crate) fn has_queued_prompts(&self) -> bool {
        self.worker.has_waiting_prompts()
            || self
                .pending_install_inputs
                .iter()
                .any(|input| matches!(input, InstallInput::Prompt(_)))
    }

    fn holds_recovery(&self) -> bool {
        self.worker.holds_recovery()
    }

    fn receive_recovery(&mut self, recovered: RecoveredTurn) {
        let prompt = QueuedPrompt::recovery(self.received_prompts, recovered);
        self.received_prompts += 1;
        self.worker.admit(prompt);
    }
}

struct ContextNotices {
    startup: Vec<String>,
    claimed: HashSet<String>,
}

impl ContextNotices {
    fn claim(&mut self, text: &str) -> Option<Notice> {
        self.claimed
            .insert(text.to_owned())
            .then(|| context_notice(text))
    }

    fn restart(&mut self) -> Vec<Notice> {
        self.claimed.clear();
        self.startup
            .iter()
            .filter(|text| self.claimed.insert((*text).clone()))
            .map(|text| context_notice(text))
            .collect()
    }
}

fn context_notice(text: &str) -> Notice {
    let body = text
        .split('\n')
        .map(|line| {
            if line == LEGACY_CONTEXT_LINE {
                ""
            } else {
                line.strip_prefix(LEGACY_CONTEXT_PREFIX).unwrap_or(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Notice::new(NoticeTone::Warning, CONTEXT_TOPIC, body)
}

pub(crate) struct Controller {
    agent: Agent,
    state: ControllerState,
    persistence: Option<Persistence>,
    questions: Option<QuestionRequests>,
    upgrade: UpgradeShortcut,
    pick_at_start: bool,
    catalog: CatalogFetch,
    herdr: Option<Arc<crate::herdr::Herdr>>,
    installation: Option<InstallTask>,
    listing: SessionListing,
    sign_in: Option<PendingSignIn>,
}

enum Wake {
    Settled,
    Command(Option<UiCommand>),
    SignIn(Result<(), ChatGptError>),
}

struct CatalogFetch {
    source: ModelSource,
    provider: String,
    pending: Option<BoxFuture<'static, ModelCatalog>>,
    waiting: Vec<ModelChange>,
    settings: Option<SettingsUpdate>,
    errand: Option<BoxFuture<'static, Vec<Notice>>>,
}

impl CatalogFetch {
    fn retarget(&mut self, source: ModelSource, provider: &str) {
        self.source = source;
        provider.clone_into(&mut self.provider);
        self.pending = None;
    }

    fn request(&mut self) {
        if self.pending.is_none() {
            let source = self.source.clone();
            self.pending = Some(Box::pin(async move { source.catalog().await }));
        }
    }

    fn change(
        &mut self,
        state: &mut ControllerState,
        persistence: &mut Option<Persistence>,
        change: ModelChange,
        work: Work,
    ) {
        let catalog = if change.needs_catalog(state) {
            self.source.cached()
        } else {
            Some(ModelCatalog::Failed { retry: None })
        };
        match catalog {
            Some(catalog) if self.waiting.is_empty() => {
                apply_change(state, persistence, change, listed(&catalog), work);
            }
            _ => {
                self.waiting.push(change);
                self.request();
            }
        }
    }

    fn run(&mut self, errand: BoxFuture<'static, Vec<Notice>>) {
        let earlier = self.errand.take();
        self.errand = Some(Box::pin(async move {
            let mut notices = match earlier {
                Some(earlier) => earlier.await,
                None => Vec::new(),
            };
            notices.extend(errand.await);
            notices
        }));
    }

    async fn next_command(
        &mut self,
        commands: &mut UnboundedReceiver<UiCommand>,
        state: &mut ControllerState,
        persistence: &mut Option<Persistence>,
        work: Work,
    ) -> Option<UiCommand> {
        loop {
            tokio::select! {
                command = commands.recv() => return command,
                catalog = finished(&mut self.pending) => self.arrived(state, persistence, catalog, work),
                notices = finished(&mut self.errand) => {
                    self.errand = None;
                    for notice in notices {
                        state.emit(UiEvent::Notice { notice });
                    }
                }
            }
        }
    }

    async fn settle(&mut self, state: &mut ControllerState, persistence: &mut Option<Persistence>) {
        if self.waiting.is_empty() && self.settings.is_none() {
            return;
        }
        if let Some(fetch) = self.pending.take() {
            let catalog = fetch.await;
            self.arrived(state, persistence, catalog, Work::Idle);
        }
    }

    fn arrived(
        &mut self,
        state: &mut ControllerState,
        persistence: &mut Option<Persistence>,
        catalog: ModelCatalog,
        work: Work,
    ) {
        self.pending = None;
        for change in std::mem::take(&mut self.waiting) {
            apply_change(state, persistence, change, listed(&catalog), work);
        }
        if let Some(update) = self.settings.take() {
            state.show_settings(update, listed(&catalog));
        }
        let provider = self.provider.clone();
        state.emit(UiEvent::ModelCatalog { provider, catalog });
        state.sync_model_controls();
    }
}

impl Controller {
    pub(crate) fn new(
        mut setup: AgentSetup,
        emit: Emit,
        persistence: Option<Persistence>,
        pick_at_start: bool,
        worker: Arc<WorkerRuntime>,
    ) -> Self {
        let questions = setup.take_question_requests();
        let notices = ContextNotices {
            startup: setup.context_notices().to_vec(),
            claimed: HashSet::new(),
        };
        let mcp = setup.mcp_host(Arc::clone(&emit));
        let setup_controls = setup.model_controls();
        let state = ControllerState {
            session_title: SessionTitle::new(Arc::clone(&emit)),
            model: setup.model().to_owned(),
            effort: setup.reasoning_effort(),
            permissions: setup.permission_runtime(Arc::clone(&emit)),
            speed: if setup.fast_mode() {
                Speed::Fast
            } else {
                Speed::Normal
            },
            statusline: setup.statusline(),
            menu_settings: MenuSettings::new(setup.prompt_history_enabled()),
            setup,
            config_pending: false,
            pending_clear: None,
            pending_install_inputs: VecDeque::new(),
            received_prompts: 0,
            worker,
            context_notices: Arc::new(Mutex::new(notices)),
            emit,
            clipboard: Arc::new(NativeClipboard),
            last_reply: None,
            history_turns: 0,
            context_to_compact: false,
            mcp,
            shown_controls: setup_controls,
            sign_in: None,
        };
        if let Some(approvals) = state.setup.approvals() {
            approvals.attach(Arc::clone(&state.emit));
        }
        Self {
            agent: state
                .setup
                .agent(persistence.is_some())
                .with_steering(Arc::clone(&state.worker)),
            catalog: CatalogFetch {
                source: state.setup.models_source(),
                provider: state.setup.provider().label().to_owned(),
                pending: None,
                waiting: Vec::new(),
                settings: None,
                errand: None,
            },
            state,
            persistence,
            questions,
            upgrade: UpgradeShortcut::default(),
            pick_at_start,
            herdr: None,
            installation: None,
            listing: SessionListing::default(),
            sign_in: None,
        }
    }

    pub(crate) fn with_herdr(mut self, herdr: Option<Arc<crate::herdr::Herdr>>) -> Self {
        self.herdr = herdr;
        self
    }

    pub(crate) fn requesting_ultrafast(mut self, requested: bool) -> Self {
        if requested {
            self.state.speed = Speed::UltraRequested;
            self.reconfigure();
        }
        self
    }

    pub(crate) fn with_upgrade(mut self, upgrade: UpgradeShortcut) -> Self {
        self.upgrade = upgrade;
        self
    }

    #[cfg(test)]
    fn with_clipboard(mut self, clipboard: Arc<dyn Clipboard>) -> Self {
        self.state.clipboard = clipboard;
        self
    }

    pub(crate) async fn run(mut self, mut commands: UnboundedReceiver<UiCommand>) {
        self.show_startup_notices();
        if !self.pick_at_start {
            let resuming = self.persistence.as_ref().is_some_and(Persistence::resuming);
            let resumed_title = self
                .persistence
                .as_ref()
                .and_then(Persistence::resumed_title);
            let (opened, continues) = self
                .persistence
                .as_mut()
                .map_or((Vec::new(), false), |persistence| {
                    persistence.open(&mut self.agent)
                });
            if let Some(title) = resumed_title {
                self.state.session_title.set(Some(&title));
            }
            self.bind_children();
            for notice in opened {
                self.session_notice(Some(notice));
            }
            self.continue_recovery(continues);
            if resuming {
                self.ask_for_a_login();
            }
        }
        if let Some(persistence) = &self.persistence {
            persistence.preload(&mut self.listing);
        }
        if let Some(herdr) = &self.herdr {
            herdr.initialize(self.persistence.as_ref().and_then(Persistence::active_id));
        }
        self.remember_agent_facts();
        self.serve(&mut commands).await;
        self.drain_installations().await;
        if let Some(persistence) = &mut self.persistence {
            persistence.close(&mut self.agent);
        }
    }

    fn next_runnable_prompt(&mut self) -> Option<QueuedPrompt> {
        if self.installation.is_some() || self.state.login_missing() || self.signing_in() {
            return None;
        }
        let prompt = self.state.worker.take_next()?;
        if prompt.recovered().is_none() {
            let settled = self
                .persistence
                .as_mut()
                .and_then(|persistence| persistence.settle_open_recovery(&mut self.agent));
            self.session_notice(settled);
        }
        Some(prompt)
    }

    async fn serve(&mut self, commands: &mut UnboundedReceiver<UiCommand>) {
        loop {
            self.state.sync_model_controls();
            if let Some(prompt) = self.next_runnable_prompt() {
                if !self.run_turn(&prompt, commands).await {
                    return;
                }
                continue;
            }
            let command = match self.next_wake(commands).await {
                Wake::Settled => continue,
                Wake::SignIn(result) => {
                    self.finish_sign_in(result).await;
                    continue;
                }
                Wake::Command(None) => return,
                Wake::Command(Some(command)) => command,
            };
            match command {
                UiCommand::Submit { prompt, skills } => {
                    observe_prompt(self.persistence.as_ref(), &prompt);
                    self.state
                        .receive_prompt(prompt, skills, self.installation.is_some());
                }
                UiCommand::RunCommand { text } => {
                    if !self.run_idle_command(&text, commands).await {
                        return;
                    }
                }
                UiCommand::ListModels => self.catalog.request(),
                UiCommand::SelectProvider { provider } => self.select_provider(&provider).await,
                UiCommand::SignIn { provider } => self.choose_login(&provider).await,
                UiCommand::ReopenSignIn => self.state.steer_sign_in(SignInControl::reopen),
                UiCommand::CancelSignIn => self.state.steer_sign_in(SignInControl::cancel),
                UiCommand::RetryHeldPrompt => self.retry_held_prompt().await,
                UiCommand::DropHeldPrompt => self.drop_held_prompts(),
                UiCommand::SelectModel {
                    model,
                    effort,
                    fast_mode,
                } => {
                    let pick = ModelPick {
                        model,
                        effort,
                        fast_mode,
                    };
                    self.change_model(ModelChange::Pick(pick)).await;
                }
                UiCommand::TogglePermissionMode => self.state.permissions.toggle_mode(),
                UiCommand::ToggleStatusline { item } => self.state.flip_statusline(item),
                UiCommand::StepSetting { setting, delta } => {
                    self.step_setting(setting, delta).await;
                }
                UiCommand::SelectModelFromSettings { model } => {
                    self.select_model_from_settings(model).await;
                }
                UiCommand::FullAccessWarningShown => {
                    self.state.permissions.full_access_warning_shown();
                }
                UiCommand::ApplyReadyUpgrade => {
                    if self.apply_ready_upgrade() {
                        return;
                    }
                }
                UiCommand::ListSessions {
                    scope,
                    after,
                    limit,
                } => self.list_sessions(scope, after, limit),
                command @ (UiCommand::OpenSessions { .. } | UiCommand::ResumeSession { .. })
                    if self.state.holds_recovery() =>
                {
                    refuse_session_command(&self.state, command);
                }
                UiCommand::OpenSessions { scope } => self.open_picker(scope),
                UiCommand::ResumeSession { id } => self.resume_session(&id),
                UiCommand::CloseSessionPicker => self.close_picker(),
                UiCommand::Cancel { .. }
                | UiCommand::PauseRecovery { .. }
                | UiCommand::Approval { .. }
                | UiCommand::QuestionAnswered { .. }
                | UiCommand::CancelCompaction => {}
            }
        }
    }

    async fn next_wake(&mut self, commands: &mut UnboundedReceiver<UiCommand>) -> Wake {
        let next =
            self.catalog
                .next_command(commands, &mut self.state, &mut self.persistence, Work::Idle);
        tokio::select! {
            result = wait_install(&mut self.installation) => {
                complete_install(&self.state, &mut self.installation, result);
                self.settle_deferred_commands(true).await;
                Wake::Settled
            }
            scanned = self.listing.scanned() => {
                self.sessions_scanned(scanned);
                Wake::Settled
            }
            command = next => Wake::Command(command),
            result = signed_in(&mut self.sign_in) => Wake::SignIn(result),
        }
    }

    async fn run_idle_command(
        &mut self,
        text: &str,
        commands: &mut UnboundedReceiver<UiCommand>,
    ) -> bool {
        if defer_install_input(&mut self.state, text, self.installation.is_some()) {
            return true;
        }
        match handle_command(&mut self.state, text, Work::Idle) {
            CommandEffect::None => {}
            CommandEffect::Install(request) => {
                self.installation = Some(request.start(&self.state, None));
            }
            CommandEffect::SwitchModel(query) => {
                self.change_model(ModelChange::Query(query)).await;
            }
            CommandEffect::Clear => self.clear(self.state.received_prompts),
            CommandEffect::ToggleFast => self.change_model(ModelChange::ToggleFast).await,
            CommandEffect::WithdrawUltrafast => {
                self.change_model(ModelChange::WithdrawUltrafast).await;
            }
            CommandEffect::Compact => return self.compact(commands).await,
            CommandEffect::OpenSessions if self.state.holds_recovery() => {
                refuse_resume_during_turn(&self.state);
            }
            CommandEffect::OpenSessions => self.open_picker(SessionScope::CurrentWorkspace),
            CommandEffect::OpenSettings => self.open_settings_menu().await,
            CommandEffect::Rename(title) => {
                rename_session(&self.state, self.persistence.as_mut(), &title);
            }
            CommandEffect::Logout(target) => self.sign_out(&target).await,
        }
        true
    }

    async fn compact(&mut self, commands: &mut UnboundedReceiver<UiCommand>) -> bool {
        self.state.worker.begin_compaction();
        self.state.compaction(CompactionActivity::Preparing);
        let cancel = CancellationToken::new();
        let emit = Arc::clone(&self.state.emit);
        let state = &mut self.state;
        let persistence = &mut self.persistence;
        let catalog = &mut self.catalog;
        let work = Work::Compaction;
        let mut open = true;
        let installation = &mut self.installation;
        let result = {
            let mut summarizing = move || {
                emit(UiEvent::CompactionActivity {
                    activity: CompactionActivity::Summarizing,
                });
            };
            let compaction = self.agent.compact(&mut summarizing, &cancel);
            tokio::pin!(compaction);
            loop {
                tokio::select! {
                    result = &mut compaction => break result,
                    result = wait_install(installation) => {
                        complete_install(state, installation, result);
                        drain_install_inputs(state, installation, &cancel);
                    },
                    command = catalog.next_command(commands, state, persistence, work), if open => match command {
                        None => {
                            open = false;
                            cancel.cancel();
                        }
                        Some(UiCommand::Submit { prompt, skills }) => {
                            observe_prompt(persistence.as_ref(), &prompt);
                            state.receive_prompt(prompt, skills, installation.is_some());
                        }
                        Some(UiCommand::CancelCompaction) => cancel.cancel(),
                        Some(command) => {
                            run_deferred(state, persistence, catalog, command, installation, work, &cancel);
                            state.sync_model_controls();
                        }
                    },
                }
            }
        };
        self.state.worker.finish_processing();
        self.state.compaction(compaction_activity(result));
        self.settle_deferred_commands(open).await;
        open
    }

    fn apply_ready_upgrade(&mut self) -> bool {
        let handoff = self
            .persistence
            .as_mut()
            .map(|persistence| persistence as &mut dyn ResumeHandoff);
        self.upgrade.apply(handoff, &*self.state.emit)
    }

    fn open_picker(&self, scope: SessionScope) {
        self.state.emit(UiEvent::SessionPickerOpened { scope });
    }

    fn list_sessions(&mut self, scope: SessionScope, after: Option<SessionCursor>, limit: usize) {
        let request = PageRequest {
            scope,
            after,
            limit,
        };
        let listed = self
            .persistence
            .as_ref()
            .map_or(Err(SessionError::SessionStoreUnavailable), |persistence| {
                persistence.list(&mut self.listing, request.clone())
            });
        match listed {
            Ok(Listed::Ready(page)) => self.state.emit(UiEvent::SessionsListed { page }),
            Ok(Listed::Waiting) => {}
            Err(error) => self.sessions_unlisted(&request, error),
        }
    }

    fn sessions_scanned(&mut self, scanned: Result<SessionCatalog, SessionError>) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        for answer in persistence.finish_listing(&mut self.listing, scanned) {
            match answer.page {
                Ok(page) => self.state.emit(UiEvent::SessionsListed { page }),
                Err(error) => self.sessions_unlisted(&answer.request, error),
            }
        }
    }

    fn sessions_unlisted(&self, request: &PageRequest, error: SessionError) {
        let action = if request.after.is_some() {
            "unable to load more saved sessions"
        } else {
            "unable to list saved sessions"
        };
        self.state
            .notice(NoticeTone::Error, "session", &format!("{action}: {error}"));
        self.state.emit(UiEvent::SessionsUnavailable {
            scope: request.scope,
        });
    }

    fn resume_session(&mut self, id: &str) {
        let Some(persistence) = &mut self.persistence else {
            self.refuse_resume(id, ResumeRefusal::Unavailable);
            return;
        };
        match persistence.resume_selected(id, &mut self.agent, &self.state.setup) {
            Ok(switched) => {
                self.forget_tracked_changes();
                self.state.setup.forget_children();
                self.bind_children();
                self.restore_preferences(switched.preferences);
                self.remember_agent_facts();
                self.state.session_title.set(switched.title.as_deref());
                self.state.emit(UiEvent::SessionResumed {
                    history: switched.history,
                });
                self.show_startup_notices();
                for notice in switched.notices {
                    self.session_notice(Some(notice));
                }
                self.continue_recovery(switched.continues);
                self.ask_for_a_login();
            }
            Err(refused) => {
                self.session_notice(refused.notice);
                self.refuse_resume(id, refused.refusal);
            }
        }
    }

    fn continue_recovery(&mut self, continues: bool) {
        let Some(persistence) = self.persistence.as_ref().filter(|_| continues) else {
            return;
        };
        if self.state.login_missing() {
            return self
                .state
                .notice(NoticeTone::Warning, RECOVERY_TOPIC, SIGN_IN_TO_CONTINUE);
        }
        match persistence.continue_recovery(
            &self.state.setup,
            &self.state.model,
            self.state.fast_mode(),
            self.state.ultrafast_requested(),
        ) {
            Ok(recovered) => {
                let recovered = RecoveredTurn {
                    source_presented: true,
                    ..recovered
                };
                observe_prompt(self.persistence.as_ref(), &recovered.prompt);
                self.state.emit(UiEvent::RecoveryContinuing {
                    prompt: recovered.prompt.clone(),
                    id: self.state.received_prompts,
                });
                self.state.receive_recovery(recovered);
            }
            Err(_) => self
                .state
                .notice(NoticeTone::Warning, RECOVERY_TOPIC, NOT_CONTINUED),
        }
    }

    fn ask_for_a_login(&self) {
        if self.state.login_missing() {
            self.state.refuse_signed_out();
        }
    }

    fn restore_preferences(&mut self, restored: RestoredPreferences) {
        self.state
            .setup
            .restore_reasoning(restored.reasoning_effort, restored.fast_mode);
        self.state.effort = self.state.setup.reasoning_effort();
        self.state
            .restore_speed(restored.fast_mode, restored.ultrafast_mode);
        if restored.model != self.state.model {
            self.state.use_model(restored.model);
        }
        self.reconfigure();
    }

    fn refuse_resume(&self, id: &str, refusal: ResumeRefusal) {
        self.state.emit(UiEvent::SessionResumeFailed {
            id: id.to_owned(),
            refusal,
        });
    }

    fn close_picker(&mut self) {
        let started = self
            .persistence
            .as_mut()
            .and_then(|persistence| persistence.begin_unless_open(&mut self.agent));
        self.bind_children();
        self.session_notice(started);
    }

    fn bind_children(&self) {
        let store = self.persistence.as_ref().and_then(Persistence::children);
        self.state.setup.bind_children(store);
    }

    fn reconfigure(&mut self) {
        let mut config = self.state.setup.config(&self.state.model);
        config.fast_mode = self.state.fast_mode();
        config.ultrafast_mode = self.state.ultrafast_requested();
        config.reasoning_effort = self.state.effort.clone().into_named();
        self.state.setup.delegate_as(&config);
        self.agent.set_config(config);
    }

    fn remember_agent_facts(&mut self) {
        self.state.last_reply = self.agent.last_assistant_reply();
        self.state.history_turns = self.agent.history_turns();
        self.state.context_to_compact = self.agent.has_context_to_compact();
    }

    fn forget_tracked_changes(&self) {
        if let Some(tracker) = self.state.change_tracker() {
            tracker.clear();
        }
    }

    async fn change_model(&mut self, change: ModelChange) {
        let catalog = if change.needs_catalog(&self.state) {
            self.catalog.source.catalog().await
        } else {
            ModelCatalog::Failed { retry: None }
        };
        let models = listed(&catalog);
        apply_change(
            &mut self.state,
            &mut self.persistence,
            change,
            models,
            Work::Idle,
        );
        self.reconfigure();
    }

    fn clear(&mut self, first_kept_prompt: u64) {
        self.agent.clear_history();
        self.forget_tracked_changes();
        self.state.setup.forget_children();
        let started = self
            .persistence
            .as_mut()
            .and_then(|persistence| persistence.begin_fresh(&mut self.agent));
        self.bind_children();
        self.state.session_title.set(None);
        self.state.worker.discard_before(first_kept_prompt);
        self.state
            .pending_install_inputs
            .retain(|input| match input {
                InstallInput::Prompt(prompt) => prompt.id >= first_kept_prompt,
                InstallInput::Skills { .. } | InstallInput::Clear(_) => true,
            });
        for prompt in self.state.worker.waiting_texts() {
            observe_prompt(self.persistence.as_ref(), &prompt);
        }
        for input in &self.state.pending_install_inputs {
            if let InstallInput::Prompt(prompt) = input {
                observe_prompt(self.persistence.as_ref(), &prompt.text);
            }
        }
        self.remember_agent_facts();
        self.state
            .emit(UiEvent::ConversationCleared { first_kept_prompt });
        self.show_startup_notices();
        self.session_notice(started);
    }

    fn session_notice(&self, notice: Option<Notice>) {
        if let Some(notice) = notice {
            self.state.emit(UiEvent::Notice { notice });
        }
    }

    fn finish_turn(&mut self, report: &TurnReport) {
        let finished = self
            .persistence
            .as_mut()
            .and_then(|persistence| persistence.finish_turn(report));
        self.session_notice(finished);
    }

    fn show_startup_notices(&mut self) {
        let restarted = self.state.lock_context_notices().restart();
        for notice in restarted {
            self.state.emit(UiEvent::Notice { notice });
        }
    }

    async fn run_turn(
        &mut self,
        prompt: &QueuedPrompt,
        commands: &mut UnboundedReceiver<UiCommand>,
    ) -> bool {
        self.state.skills().refresh();
        self.start_title_generation(&prompt.text);
        let cancel = CancellationToken::new();
        let pause = self.agent.recovery_pause();
        let running = Arc::new(Mutex::new(None));
        let running_turn = || *running.lock().unwrap_or_else(PoisonError::into_inner);
        let mut events = turn_events(
            Arc::clone(&self.state.emit),
            Arc::clone(&running),
            self.state.setup.approvals().cloned(),
            Arc::clone(&self.state.context_notices),
        );
        let (recovering, mut recoveries) = unbounded_channel();
        let mut sink = move |event: UiEvent| {
            if matches!(event, UiEvent::Recovery { .. }) {
                let _ = recovering.send(());
            }
            events(event);
        };
        let source = self.state.setup.models_source();
        let ready = source.ready();
        tokio::pin!(ready);
        let mut controls_ready = false;
        let state = &mut self.state;
        let persistence = &mut self.persistence;
        let questions = &mut self.questions;
        let catalog = &mut self.catalog;
        let work = Work::Turn;
        let mut open = true;
        let installation = &mut self.installation;
        let report = {
            let turn = run_prompt(&mut self.agent, prompt, &mut sink, &cancel);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    report = &mut turn => break report,
                    () = &mut ready, if !controls_ready => {
                        controls_ready = true;
                        state.sync_model_controls();
                    }
                    result = wait_install(installation) => {
                        complete_install(state, installation, result);
                        drain_install_inputs(state, installation, &cancel);
                    },
                    command = catalog.next_command(commands, state, persistence, work), if open => match command {
                        None => {
                            open = false;
                            cancel.cancel();
                        }
                        Some(UiCommand::Submit { prompt, skills }) => {
                            observe_prompt(persistence.as_ref(), &prompt);
                            state.receive_prompt(prompt, skills, installation.is_some());
                        }
                        Some(UiCommand::Cancel { turn_id }) => {
                            if running_turn() == Some(turn_id) {
                                state.worker.request_cancel();
                                cancel.cancel();
                            }
                        }
                        Some(UiCommand::PauseRecovery { turn_id }) => {
                            if running_turn() == Some(turn_id) {
                                pause.request();
                                cancel.cancel();
                            }
                        }
                        Some(UiCommand::Approval { request_id, answer }) => {
                            state.setup.answer_approval(request_id, answer);
                        }
                        Some(UiCommand::QuestionAnswered { request_id, answers }) => {
                            if let Some(questions) = state.setup.questions() {
                                questions.resolve(request_id, answers);
                            }
                        }
                        Some(command) => {
                            run_deferred(state, persistence, catalog, command, installation, work, &cancel);
                            state.sync_model_controls();
                        }
                    },
                    request = next_question(questions) => relay_question(state, running_turn(), request),
                    Some(()) = recoveries.recv() => {
                        if let Some(notice) = persistence.as_mut().and_then(Persistence::remember_durable_work) {
                            state.emit(UiEvent::Notice { notice });
                        }
                    }
                }
            }
        };
        self.state.setup.end_turn_approvals();
        self.state.worker.finish_processing();
        if let Some(turn_id) = running_turn() {
            self.announce_turn_end(turn_id, &report);
        }
        self.finish_turn(&report);
        self.remember_session_title(&prompt.text);
        self.settle_deferred_commands(open).await;
        open
    }

    async fn drain_installations(&mut self) {
        loop {
            self.state.worker.clear();
            self.state
                .pending_install_inputs
                .retain(|input| !matches!(input, InstallInput::Prompt(_)));
            self.settle_deferred_commands(false).await;
            if self.installation.is_none() {
                break;
            }
            finish_install(&self.state, &mut self.installation).await;
        }
    }

    fn announce_turn_end(&self, turn_id: TurnId, report: &TurnReport) {
        let source = self.state.setup.source();
        let status = match &report.failure {
            Some(TurnFailure::Persistence(_)) if report.outcome != TurnOutcome::Failed => None,
            failure => failure
                .as_ref()
                .and_then(|failure| failure_status(failure, source)),
        };
        if let Some(text) = status {
            self.state.emit(UiEvent::ApiStatus { turn_id, text });
        }
        self.state.emit(UiEvent::TurnFinished {
            turn_id,
            outcome: report.outcome,
        });
    }

    fn start_title_generation(&mut self, prompt: &str) {
        let untitled = self.agent.history_turns() == 0 && self.state.session_title.is_untitled();
        if let Some(persistence) = &mut self.persistence {
            persistence.start_title_generation(
                &self.state.setup,
                prompt,
                untitled,
                &self.state.session_title,
            );
        }
    }

    fn remember_session_title(&self, prompt: &str) {
        if self.state.session_title.is_untitled() && self.agent.history_turns() > 0 {
            self.state
                .session_title
                .set(Some(&prompt_display_title(prompt)));
        }
    }

    async fn settle_deferred_commands(&mut self, open: bool) {
        if open {
            self.catalog
                .settle(&mut self.state, &mut self.persistence)
                .await;
        }
        self.remember_agent_facts();
        if std::mem::take(&mut self.state.config_pending) {
            self.reconfigure();
        }
        if let Some(first_kept_prompt) = self.state.pending_clear.take() {
            self.clear(first_kept_prompt);
        }
        loop {
            drain_install_inputs(
                &mut self.state,
                &mut self.installation,
                &CancellationToken::new(),
            );
            let Some(first_kept_prompt) = self.state.pending_clear.take() else {
                break;
            };
            self.clear(first_kept_prompt);
        }
    }
}

fn defer_install_input(state: &mut ControllerState, text: &str, installing: bool) -> bool {
    if !installing {
        return false;
    }
    let Some(command) = SLASH_REGISTRY.parse_command(text) else {
        return false;
    };
    let input = match command.kind {
        SlashKind::Skills => InstallInput::Skills {
            rest: command.payload.to_owned(),
            accepted: is_install_command(text).then(|| state.skills().installations().start()),
        },
        SlashKind::ClearScreen | SlashKind::NewSession | SlashKind::ResetSession => {
            InstallInput::Clear(state.received_prompts)
        }
        _ => return false,
    };
    state.pending_install_inputs.push_back(input);
    true
}

fn drain_install_inputs(
    state: &mut ControllerState,
    installation: &mut Option<InstallTask>,
    cancel: &CancellationToken,
) {
    while installation.is_none() {
        let Some(input) = state.pending_install_inputs.pop_front() else {
            break;
        };
        match input {
            InstallInput::Skills { rest, accepted } => {
                if let Some(request) = handle_skills(state, &rest) {
                    *installation = Some(request.start(state, accepted));
                }
            }
            InstallInput::Clear(first_kept) => {
                state.pending_clear = Some(first_kept);
                state.worker.clear();
                state.worker.request_cancel();
                cancel.cancel();
            }
            InstallInput::Prompt(prompt) => state.worker.admit(prompt),
        }
    }
}

async fn finished<T>(slot: &mut Option<BoxFuture<'static, T>>) -> T {
    match slot {
        Some(future) => future.await,
        None => std::future::pending().await,
    }
}

async fn next_question(requests: &mut Option<QuestionRequests>) -> QuestionRequest {
    match requests {
        Some(requests) => requests.next().await,
        None => std::future::pending().await,
    }
}

fn relay_question(state: &ControllerState, turn: Option<TurnId>, request: QuestionRequest) {
    match turn {
        Some(turn_id) => state.emit(UiEvent::QuestionRequested { turn_id, request }),
        None => {
            if let Some(questions) = state.setup.questions() {
                questions.resolve(request.id, None);
            }
        }
    }
}

fn run_deferred(
    state: &mut ControllerState,
    persistence: &mut Option<Persistence>,
    catalog: &mut CatalogFetch,
    command: UiCommand,
    installation: &mut Option<InstallTask>,
    work: Work,
    cancel: &CancellationToken,
) {
    if let UiCommand::RunCommand { text } = &command
        && defer_install_input(state, text, installation.is_some())
    {
        return;
    }
    let change = match command {
        UiCommand::RunCommand { text } => match handle_command(state, &text, work) {
            CommandEffect::None | CommandEffect::Compact | CommandEffect::OpenSessions => return,
            CommandEffect::Install(request) => {
                *installation = Some(request.start(state, None));
                return;
            }
            CommandEffect::SwitchModel(query) => ModelChange::Query(query),
            CommandEffect::ToggleFast => ModelChange::ToggleFast,
            CommandEffect::WithdrawUltrafast => ModelChange::WithdrawUltrafast,
            CommandEffect::OpenSettings => return catalog.open_settings_menu(state),
            CommandEffect::Rename(title) => {
                return rename_session(state, persistence.as_mut(), &title);
            }
            CommandEffect::Logout(target) => return state.sign_out_during_work(catalog, &target),
            CommandEffect::Clear => {
                state.pending_clear = Some(state.received_prompts);
                state.worker.clear();
                state.worker.request_cancel();
                cancel.cancel();
                return;
            }
        },
        UiCommand::SelectModel {
            model,
            effort,
            fast_mode,
        } => ModelChange::Pick(ModelPick {
            model,
            effort,
            fast_mode,
        }),
        UiCommand::ListModels => return catalog.request(),
        UiCommand::SelectProvider { .. } => return state.provider_busy(),
        UiCommand::SignIn { provider } => return state.sign_in_busy(&provider),
        UiCommand::ReopenSignIn => return state.steer_sign_in(SignInControl::reopen),
        UiCommand::CancelSignIn => return state.steer_sign_in(SignInControl::cancel),
        UiCommand::TogglePermissionMode => return state.permissions.toggle_mode(),
        UiCommand::ToggleStatusline { item } => return state.flip_statusline(item),
        UiCommand::StepSetting { setting, delta } => {
            return catalog.step_setting(state, persistence, setting, delta, work);
        }
        UiCommand::SelectModelFromSettings { model } => {
            return catalog.select_model_from_settings(state, persistence, model, work);
        }
        UiCommand::FullAccessWarningShown => {
            return state.permissions.full_access_warning_shown();
        }
        UiCommand::OpenSessions { .. }
        | UiCommand::ListSessions { .. }
        | UiCommand::ResumeSession { .. }
        | UiCommand::CloseSessionPicker => return refuse_session_command(state, command),
        UiCommand::Submit { .. }
        | UiCommand::RetryHeldPrompt
        | UiCommand::DropHeldPrompt
        | UiCommand::Cancel { .. }
        | UiCommand::PauseRecovery { .. }
        | UiCommand::Approval { .. }
        | UiCommand::QuestionAnswered { .. }
        | UiCommand::CancelCompaction
        | UiCommand::ApplyReadyUpgrade => return,
    };
    catalog.change(state, persistence, change, work);
}

fn apply_change(
    state: &mut ControllerState,
    persistence: &mut Option<Persistence>,
    change: ModelChange,
    models: &[ModelOption],
    work: Work,
) {
    let saves_ultrafast = change.saves_ultrafast();
    let turns_ultrafast_off = matches!(change, ModelChange::WithdrawUltrafast);
    let (fast_before, ultrafast_before) = (state.fast_mode(), state.ultrafast_requested());
    let Outcome::Changed { effort } = change_model(state, change, models, work) else {
        return;
    };
    state.config_pending = true;
    let ultrafast = state.ultrafast_requested();
    if let Some(persistence) = persistence.as_mut()
        && (turns_ultrafast_off
            || (state.fast_mode() && !fast_before)
            || (ultrafast_before && !ultrafast))
    {
        persistence.withdraw_launch_ultrafast();
    }
    let ultrafast = saves_ultrafast.then_some(ultrafast);
    if let Some(notice) = save_session_preferences(state, persistence, effort.as_ref(), ultrafast) {
        state.emit(UiEvent::Notice { notice });
    }
}

fn refuse_session_command(state: &ControllerState, command: UiCommand) {
    match command {
        UiCommand::ListSessions { scope, .. } => {
            state.emit(UiEvent::SessionsUnavailable { scope });
        }
        UiCommand::ResumeSession { id } => state.emit(UiEvent::SessionResumeFailed {
            id,
            refusal: ResumeRefusal::Unavailable,
        }),
        UiCommand::OpenSessions { .. } => refuse_resume_during_turn(state),
        _ => {}
    }
}

async fn run_prompt(
    agent: &mut Agent,
    prompt: &QueuedPrompt,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> TurnReport {
    match prompt.recovered().cloned() {
        Some(recovered) => agent.continue_turn(recovered, events, cancel).await,
        None => {
            agent
                .run_turn_with_skills(&prompt.text, &prompt.skills, events, cancel)
                .await
        }
    }
}

fn observe_prompt(persistence: Option<&Persistence>, prompt: &str) {
    if let Some(persistence) = persistence {
        persistence.observe_prompt(prompt);
    }
}

fn save_session_preferences(
    state: &ControllerState,
    persistence: &mut Option<Persistence>,
    effort: Option<&ReasoningEffort>,
    ultrafast_mode: Option<bool>,
) -> Option<Notice> {
    persistence.as_mut().and_then(|persistence| {
        persistence.select_model(&state.model, effort, state.fast_mode(), ultrafast_mode)
    })
}

fn turn_events(
    emit: Emit,
    started: Arc<Mutex<Option<TurnId>>>,
    approvals: Option<Arc<ApprovalQueue>>,
    notices: Arc<Mutex<ContextNotices>>,
) -> impl FnMut(UiEvent) + Send {
    move |event: UiEvent| match event {
        UiEvent::TurnFinished { .. } => {}
        UiEvent::TurnStarted { turn_id } => {
            *started.lock().unwrap_or_else(PoisonError::into_inner) = Some(turn_id);
            if let Some(approvals) = &approvals {
                approvals.turn_started(turn_id);
            }
            emit(event);
        }
        UiEvent::ApprovalRequested { turn_id, request } => match &approvals {
            Some(approvals) => approvals.own(turn_id, *request),
            None => emit(UiEvent::ApprovalRequested { turn_id, request }),
        },
        UiEvent::ContextNotice { text, .. } => {
            let claimed = notices
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .claim(&text);
            if let Some(notice) = claimed {
                emit(UiEvent::Notice { notice });
            }
        }
        event => emit(event),
    }
}

fn compaction_activity(result: Result<Compaction, CompactionError>) -> CompactionActivity {
    let end = match result {
        Ok(Compaction::Compacted) => return CompactionActivity::Compacted,
        Ok(Compaction::Unchanged) | Err(CompactionError::NothingToCompact) => {
            CompactionEnd::NothingToCompact
        }
        Err(CompactionError::Cancelled) => CompactionEnd::Cancelled,
        Err(CompactionError::ContextCapacityExceeded) => CompactionEnd::ContextTooLarge,
        Err(
            CompactionError::ModelFailed
            | CompactionError::SummaryIncomplete
            | CompactionError::EmptySummary
            | CompactionError::InvalidCheckpoint
            | CompactionError::NotSaved,
        ) => CompactionEnd::Failed,
    };
    CompactionActivity::Ended(end)
}

fn failure_status(failure: &TurnFailure, source: CredentialSource) -> Option<String> {
    match failure {
        TurnFailure::Provider(error) => Some(provider_status(error, source)),
        TurnFailure::StepLimitReached
        | TurnFailure::RepeatedMalformedArguments
        | TurnFailure::RepeatedShellExecutionFailure
        | TurnFailure::RecoveryPaused => None,
        _ => Some(format!("⚠ {}", failure.code())),
    }
}

fn provider_status(error: &ProviderError, source: CredentialSource) -> String {
    match (
        ofx_gateway::http_failure(error, source.label()),
        &error.detail,
    ) {
        (Some(failure), _) if failure.unauthorized => {
            format!("⚠ {} · {}", failure.message, source.repair())
        }
        (Some(failure), _) => format!("⚠ {}", failure.message),
        (None, Some(detail)) => format!("⚠ request failed: {} · {detail}", error.code),
        (None, None) => format!("⚠ request failed: {}", error.code),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use ofx_config::{PrivateDir, ProfilePaths, Settings};
    use ofx_contract::{
        ApprovalDecision, ApprovalOrigin, ApprovalRequest, FastModeSetting, PermissionMode,
        ProviderErrorKind, SettingId, SettingsSnapshot, SkillMenuFocus, StatuslineItem,
        StatuslineToggles, ToolResultStatus, TurnId, TurnOutcome,
    };
    use ofx_exec::{ManagedExecutions, SessionSupervisor};
    use ofx_gateway::{CODEX_TITLE_MODEL, CodexEndpoints, CodexModelsEndpoints};
    use ofx_session::{ResumeTarget, SessionPreferences, SessionStore};
    use ofx_testkit::{
        FakeServer, Gate, RecordedRequest, Reply, chat_text_events, chat_tool_call_events,
    };
    use serde_json::{Value, json};
    use tokio::sync::mpsc::UnboundedSender;
    use tokio::time::timeout;

    use super::*;
    use crate::app_bootstrap_runtime::{Launch, Profile};
    use crate::app_session_runtime::{
        LaunchOverrides, ResumedSession, Resumption, open_store, session_route,
    };
    use crate::codex_provider::SubscriptionEndpoints;

    mod sign_out;
    mod steering;
    mod workspace;

    struct Harness {
        home: tempfile::TempDir,
        commands: UnboundedSender<UiCommand>,
        events: UnboundedReceiver<UiEvent>,
        seen: Vec<UiEvent>,
        clipboard: Arc<TestClipboard>,
        worker: Arc<WorkerRuntime>,
    }

    #[derive(Default)]
    struct TestClipboard {
        copied: Mutex<Vec<String>>,
        fails: AtomicBool,
    }

    impl TestClipboard {
        fn copied(&self) -> Vec<String> {
            self.copied.lock().unwrap().clone()
        }
    }

    impl Clipboard for TestClipboard {
        fn copy(&self, text: &str) -> bool {
            self.copied.lock().unwrap().push(text.to_owned());
            !self.fails.load(Ordering::SeqCst)
        }
    }

    fn local_settings(server: &FakeServer) -> Value {
        json!({
            "provider": "local",
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": server.base_url(),
                    "auth": {"type": "none"},
                    "models": ["model-a", "vendor/model-b"]
                }
            }
        })
    }

    async fn agent_setup(home: &tempfile::TempDir, server: &FakeServer) -> AgentSetup {
        agent_setup_with(
            home,
            &local_settings(server),
            SubscriptionEndpoints::default(),
        )
        .await
    }

    async fn agent_setup_with(
        home: &tempfile::TempDir,
        settings: &Value,
        endpoints: SubscriptionEndpoints,
    ) -> AgentSetup {
        profile_setup(home, settings, endpoints).await.1
    }

    async fn profile_setup(
        home: &tempfile::TempDir,
        settings: &Value,
        endpoints: SubscriptionEndpoints,
    ) -> (Profile, AgentSetup) {
        let config = home.path().join("config");
        let workspace = home.path().join("workspace");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        fs::write(config.join("settings.json"), settings.to_string()).unwrap();
        let paths = ProfilePaths {
            config,
            data: home.path().join("data"),
            state: home.path().join("state"),
            cache: home.path().join("cache"),
        };
        let settings = Settings::load(&paths, &workspace).unwrap();
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        let profile =
            Profile::new(workspace, Some(home.path().into()), Some(paths), settings).unwrap();
        let setup = profile
            .connect_interactive(
                Launch {
                    model: None,
                    permission_mode: PermissionMode::Auto,
                    system_prompt: None,
                    reasoning_effort: None,
                    fast_mode: None,
                    context_limits: &[],
                    command_timeout: None,
                    executions: &executions,
                    endpoints,
                    web_fetch_progress: None,
                    mode: None,
                    permission_prompts: false,
                    open_browser: false,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        (profile, setup)
    }

    const CODEX_MODEL: &str = "gpt-6.1-sol";
    const OTHER_CODEX_MODEL: &str = "gpt-6.1-luna";

    fn codex_settings() -> Value {
        json!({"provider": "codex", "models": {"codex": CODEX_MODEL}})
    }

    fn codex_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("data");
        fs::create_dir_all(&data).unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        let session = json!({
            "version": 1,
            "access_token": "eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl",
            "refresh_token": "rt-refresh-secret-0123456789",
            "expires_at_ms": 4_102_444_800_000_i64,
            "account_id": "acct_test",
        });
        let file = data.join("chatgpt-auth.json");
        fs::write(&file, format!("{session}\n")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        home
    }

    fn codex_endpoints(codex: &FakeServer, catalog: &FakeServer) -> SubscriptionEndpoints {
        SubscriptionEndpoints {
            codex: CodexEndpoints {
                responses: format!("{}/backend-api/codex/responses", codex.base_url()),
            },
            models: CodexModelsEndpoints {
                models: format!("{}/backend-api/codex/models", catalog.base_url()),
                client_version: format!("{}/@openai/codex/latest", catalog.base_url()),
            },
            ..SubscriptionEndpoints::default()
        }
    }

    fn catalog_version() -> Reply {
        Reply::status(200, json!({"version": "0.153.1"}).to_string())
    }

    fn catalog_listing(fast: bool) -> Reply {
        let tiers: &[&str] = if fast { &["fast"] } else { &[] };
        let model = |slug: &str| {
            json!({
                "slug": slug,
                "visibility": "list",
                "supported_in_api": true,
                "supported_reasoning_levels": [{"effort": "low"}],
                "additional_speed_tiers": tiers,
            })
        };
        let listing = json!({"models": [model(CODEX_MODEL), model(OTHER_CODEX_MODEL)]});
        Reply::status(200, listing.to_string())
    }

    fn codex_catalog(fast: bool, lookups: usize) -> FakeServer {
        let mut replies = vec![catalog_version()];
        replies.extend((0..lookups).map(|_| catalog_listing(fast)));
        FakeServer::start(replies)
    }

    fn codex_text(text: &str) -> Reply {
        let events = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","phase":"final_answer"}}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":text}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":20,"output_tokens":3}}}),
        ]
        .map(|event| event.to_string());
        Reply::sse(&events)
    }

    fn codex_partial() -> Reply {
        let events = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","phase":"final_answer"}}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":"partial\n"}),
        ]
        .map(|event| event.to_string());
        Reply::held_sse(&events)
    }

    impl Harness {
        async fn start(server: &FakeServer) -> Self {
            let home = tempfile::tempdir().unwrap();
            let setup = agent_setup(&home, server).await;
            Self::with_setup(home, setup)
        }

        async fn codex(codex: &FakeServer, catalog: &FakeServer) -> Self {
            let home = codex_home();
            let setup =
                agent_setup_with(&home, &codex_settings(), codex_endpoints(codex, catalog)).await;
            Self::with_setup(home, setup)
        }

        async fn codex_saved(codex: &FakeServer, catalog: &FakeServer, settings: &Value) -> Self {
            Self::codex_saved_in(codex_home(), codex, catalog, settings).await
        }

        async fn codex_saved_in(
            home: tempfile::TempDir,
            codex: &FakeServer,
            catalog: &FakeServer,
            settings: &Value,
        ) -> Self {
            let setup = agent_setup_with(&home, settings, codex_endpoints(codex, catalog)).await;
            Self::saved(home, setup)
        }

        async fn start_saved(server: &FakeServer) -> Self {
            let home = tempfile::tempdir().unwrap();
            let setup = agent_setup(&home, server).await;
            Self::saved(home, setup)
        }

        async fn upgrading(
            server: &FakeServer,
            upgrade: UpgradeShortcut,
            launch_ultrafast: Option<bool>,
        ) -> Self {
            let home = tempfile::tempdir().unwrap();
            let setup = agent_setup(&home, server).await;
            Self::saved_with(home, setup, upgrade, launch_ultrafast)
        }

        fn saved(home: tempfile::TempDir, setup: AgentSetup) -> Self {
            Self::saved_with(home, setup, UpgradeShortcut::default(), None)
        }

        fn saved_with(
            home: tempfile::TempDir,
            setup: AgentSetup,
            upgrade: UpgradeShortcut,
            launch_ultrafast: Option<bool>,
        ) -> Self {
            let workspace = fs::canonicalize(home.path().join("workspace")).unwrap();
            let store = SessionStore::open(&home.path().join("data"), workspace.to_str().unwrap())
                .unwrap()
                .with_fx_home(home.path().to_path_buf());
            let route = session_route(&setup).unwrap();
            let preferences = SessionPreferences {
                provider: route.provider.clone(),
                model: setup.configured_model().to_owned(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
                ultrafast_mode: false,
            };
            let overrides = LaunchOverrides {
                model: None,
                effort: None,
                fast_mode: None,
                ultrafast_mode: launch_ultrafast,
            };
            let persistence = Persistence::new(store, route, preferences, overrides, None);
            let requested = launch_ultrafast == Some(true);
            Self::spawn(home, setup, Some(persistence), upgrade, requested)
        }

        fn with_setup(home: tempfile::TempDir, setup: AgentSetup) -> Self {
            Self::spawn(home, setup, None, UpgradeShortcut::default(), false)
        }

        async fn resuming(
            home: tempfile::TempDir,
            settings: &Value,
            endpoints: SubscriptionEndpoints,
            id: &str,
        ) -> Self {
            let (mut profile, setup) = profile_setup(&home, settings, endpoints).await;
            let store = open_store(&profile).unwrap();
            let Ok(session) =
                ResumedSession::open(&store, &mut profile, &ResumeTarget::Id(id.to_owned()))
            else {
                panic!("the saved session reopens");
            };
            let route = session_route(&setup).unwrap();
            let preferences = SessionPreferences {
                provider: route.provider.clone(),
                model: setup.configured_model().to_owned(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
                ultrafast_mode: false,
            };
            let overrides = LaunchOverrides {
                model: None,
                effort: None,
                fast_mode: None,
                ultrafast_mode: None,
            };
            let resumption = Resumption {
                session,
                remember: false,
            };
            let persistence =
                Persistence::new(store, route, preferences, overrides, Some(resumption));
            Self::spawn(
                home,
                setup,
                Some(persistence),
                UpgradeShortcut::default(),
                false,
            )
        }

        async fn finish(self) -> tempfile::TempDir {
            let Self {
                home,
                commands,
                mut events,
                ..
            } = self;
            drop(commands);
            while events.recv().await.is_some() {}
            home
        }

        fn with_setup_observer(
            home: tempfile::TempDir,
            setup: AgentSetup,
            observe: impl Fn(&UiEvent) + Send + Sync + 'static,
        ) -> Self {
            Self::spawn_observer(
                home,
                setup,
                None,
                false,
                UpgradeShortcut::default(),
                observe,
            )
        }

        fn requesting_ultrafast(home: tempfile::TempDir, setup: AgentSetup) -> Self {
            Self::spawn_observer(home, setup, None, true, UpgradeShortcut::default(), |_| {})
        }

        fn spawn(
            home: tempfile::TempDir,
            setup: AgentSetup,
            persistence: Option<Persistence>,
            upgrade: UpgradeShortcut,
            ultrafast: bool,
        ) -> Self {
            Self::spawn_observer(home, setup, persistence, ultrafast, upgrade, |_| {})
        }

        fn spawn_observer(
            home: tempfile::TempDir,
            setup: AgentSetup,
            persistence: Option<Persistence>,
            ultrafast: bool,
            upgrade: UpgradeShortcut,
            observe: impl Fn(&UiEvent) + Send + Sync + 'static,
        ) -> Self {
            let (events_sender, events) = unbounded_channel();
            let emit: Emit = Arc::new(move |event| {
                observe(&event);
                let _ = events_sender.send(event);
            });
            let (commands, receiver) = unbounded_channel();
            let clipboard = Arc::new(TestClipboard::default());
            let shared: Arc<dyn Clipboard> = clipboard.clone();
            let worker = Arc::new(WorkerRuntime::default());
            tokio::spawn(
                Controller::new(setup, emit, persistence, false, Arc::clone(&worker))
                    .requesting_ultrafast(ultrafast)
                    .with_clipboard(shared)
                    .with_upgrade(upgrade)
                    .run(receiver),
            );
            Self {
                home,
                commands,
                events,
                seen: Vec::new(),
                clipboard,
                worker,
            }
        }

        fn send(&self, command: UiCommand) {
            self.commands.send(command).unwrap();
        }

        fn command(&self, text: &str) {
            self.send(UiCommand::RunCommand {
                text: text.to_owned(),
            });
        }

        fn submit(&self, prompt: &str) {
            self.send(UiCommand::Submit {
                prompt: prompt.to_owned(),
                skills: Vec::new(),
            });
        }

        fn running_turn(&self) -> TurnId {
            self.seen
                .iter()
                .rev()
                .find_map(|event| match event {
                    UiEvent::TurnStarted { turn_id } => Some(*turn_id),
                    _ => None,
                })
                .unwrap()
        }

        async fn until(&mut self, done: impl Fn(&UiEvent) -> bool) -> &[UiEvent] {
            let start = self.seen.len();
            while let Some(event) = self.events.recv().await {
                let finished = done(&event);
                self.seen.push(event);
                if finished {
                    break;
                }
            }
            &self.seen[start..]
        }
    }

    fn finished(outcome: TurnOutcome) -> impl Fn(&UiEvent) -> bool {
        move |event| matches!(event, UiEvent::TurnFinished { outcome: seen, .. } if *seen == outcome)
    }

    fn notice_body(events: &[UiEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                UiEvent::Notice { notice } => Some(format!("{}|{}", notice.topic, notice.body)),
                _ => None,
            })
            .collect()
    }

    fn user_messages(body: &Value) -> usize {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .count()
    }

    #[tokio::test]
    async fn model_commands_show_and_switch_the_connection_models() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["ok"]))]);
        let mut harness = Harness::start(&server).await;
        harness.command("/model");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(notice_body(shown), ["model|model-a"]);
        harness.command("/model model-b");
        let switched = harness
            .until(|event| matches!(event, UiEvent::ModelSelected { .. }))
            .await;
        assert_eq!(
            switched.last(),
            Some(&UiEvent::ModelSelected {
                model: "vendor/model-b".to_owned()
            })
        );
        assert_eq!(notice_body(switched), ["|Switched to vendor/model-b"]);
        harness.submit("go");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(server.requests()[0].json()["model"], "vendor/model-b");
    }

    #[tokio::test]
    async fn models_switched_mid_turn_apply_to_the_next_turn() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([held, Reply::sse(&chat_text_events(&["ok"]))]);
        let mut harness = Harness::start(&server).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        harness.command("/model model-b");
        let notice = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(notice_body(notice), ["|Next turn will use vendor/model-b"]);
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
        harness.submit("next");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = server.requests();
        assert_eq!(requests[0].json()["model"], "model-a");
        assert_eq!(requests[1].json()["model"], "vendor/model-b");
    }

    #[tokio::test]
    async fn clear_starts_a_fresh_conversation() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["one"])),
            Reply::sse(&chat_text_events(&["two"])),
        ]);
        let mut harness = Harness::start(&server).await;
        harness.submit("first");
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.command("/clear");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        harness.submit("second");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(user_messages(&server.requests()[1].json()), 1);
    }

    #[tokio::test]
    async fn reset_starts_a_fresh_conversation_like_clear() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["one"])),
            Reply::sse(&chat_text_events(&["two"])),
        ]);
        let mut harness = Harness::start(&server).await;
        harness.submit("first");
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.command("/reset");
        let cleared = harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        assert_eq!(
            cleared.last(),
            Some(&UiEvent::ConversationCleared {
                first_kept_prompt: 1
            })
        );
        harness.submit("second");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(user_messages(&server.requests()[1].json()), 1);
    }

    #[tokio::test]
    async fn stats_ask_the_shell_for_its_renderer_counts_even_during_a_turn() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([held]);
        let mut harness = Harness::start(&server).await;
        harness.command("/stats");
        let idle = harness
            .until(|event| matches!(event, UiEvent::StatsRequested))
            .await;
        assert_eq!(idle, [UiEvent::StatsRequested]);
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        harness.command("/stats");
        harness
            .until(|event| matches!(event, UiEvent::StatsRequested))
            .await;
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
    }

    async fn copy_notice(harness: &mut Harness) -> Notice {
        harness.command("/copy");
        let shown = harness
            .until(
                |event| matches!(event, UiEvent::Notice { notice } if notice.topic == "clipboard"),
            )
            .await;
        let Some(UiEvent::Notice { notice }) = shown.last() else {
            unreachable!()
        };
        notice.clone()
    }

    #[tokio::test]
    async fn copy_puts_the_last_completed_reply_on_the_clipboard() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["First ", "answer."])),
            Reply::sse(&chat_text_events(&["Second answer."])),
        ]);
        let mut harness = Harness::start(&server).await;
        let empty = copy_notice(&mut harness).await;
        assert_eq!(
            (empty.tone, empty.body.as_str()),
            (NoticeTone::Neutral, "No assistant reply to copy.")
        );
        assert!(harness.clipboard.copied().is_empty());
        harness.submit("first");
        harness.until(finished(TurnOutcome::Completed)).await;
        let copied = copy_notice(&mut harness).await;
        assert_eq!(
            (copied.tone, copied.body.as_str()),
            (NoticeTone::Neutral, "Copied to clipboard.")
        );
        harness.submit("second");
        harness.until(finished(TurnOutcome::Completed)).await;
        copy_notice(&mut harness).await;
        assert_eq!(
            harness.clipboard.copied(),
            ["First answer.", "Second answer."]
        );
    }

    #[tokio::test]
    async fn copy_reports_a_clipboard_that_refuses_the_text() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["answer"]))]);
        let mut harness = Harness::start(&server).await;
        harness.clipboard.fails.store(true, Ordering::SeqCst);
        harness.submit("go");
        harness.until(finished(TurnOutcome::Completed)).await;
        let failed = copy_notice(&mut harness).await;
        assert_eq!(
            (failed.tone, failed.body.as_str()),
            (NoticeTone::Error, "Failed to copy to clipboard.")
        );
        assert_eq!(harness.clipboard.copied(), ["answer"]);
    }

    #[tokio::test]
    async fn copy_during_a_turn_takes_the_previous_reply_and_clear_forgets_it() {
        let held = Reply::held_sse(&chat_text_events(&["streaming\n"])[..2]);
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["Done before."])), held]);
        let mut harness = Harness::start(&server).await;
        harness.submit("first");
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        copy_notice(&mut harness).await;
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
        copy_notice(&mut harness).await;
        assert_eq!(harness.clipboard.copied(), ["Done before.", "Done before."]);
        harness.command("/clear");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        let cleared = copy_notice(&mut harness).await;
        assert_eq!(cleared.body, "No assistant reply to copy.");
    }

    async fn status_notice(harness: &mut Harness) -> String {
        harness.command("/status");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { notice } if notice.topic == "status"))
            .await;
        notice_body(shown).pop().unwrap()
    }

    #[tokio::test]
    async fn status_reports_the_connection_mode_workspace_and_conversation() {
        let read = Reply::sse(&chat_tool_call_events(
            "call-1",
            "read_file",
            r#"{"path":"../outside.txt"}"#,
        ));
        let server = FakeServer::start([read, Reply::sse(&chat_text_events(&["done"]))]);
        let mut harness = Harness::start(&server).await;
        fs::write(harness.home.path().join("outside.txt"), "notes\n").unwrap();
        let workspace = fs::canonicalize(harness.home.path().join("workspace")).unwrap();
        let expected = |turns: usize, grants: usize| {
            format!(
                "status|model=model-a\nmodel_source=local\nprovider_endpoint={}\nauth=configured provider\nconnected_providers=local\nauth_refreshable=false\npermission_mode=auto\nworkspace={}\nhistory_turns={turns}\nsession_permission_grants={grants}\nagent_step_limit=0\nultrafast_requested=false",
                server.base_url(),
                workspace.display()
            )
        };
        assert_eq!(status_notice(&mut harness).await, expected(0, 0));
        harness.submit("read it");
        let Some(UiEvent::ApprovalRequested { request, .. }) = harness
            .until(|event| matches!(event, UiEvent::ApprovalRequested { .. }))
            .await
            .last()
            .cloned()
        else {
            unreachable!()
        };
        assert_eq!(status_notice(&mut harness).await, expected(0, 0));
        harness.send(UiCommand::Approval {
            request_id: request.id,
            answer: ApprovalDecision::Always.into(),
        });
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(status_notice(&mut harness).await, expected(1, 1));
        harness.command("/model model-b");
        let switched = status_notice(&mut harness).await;
        assert!(
            switched.starts_with("status|model=vendor/model-b\n"),
            "{switched}"
        );
        harness.command("/clear");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        let cleared = status_notice(&mut harness).await;
        assert!(
            cleared.contains("\nhistory_turns=0\nsession_permission_grants=0\n"),
            "{cleared}"
        );
    }

    #[tokio::test]
    async fn status_names_the_codex_subscription() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(true, 1);
        let mut harness = Harness::codex(&codex, &catalog).await;
        let status = status_notice(&mut harness).await;
        assert!(
            status.starts_with(&format!(
                "status|model={CODEX_MODEL}\nmodel_source=Codex subscription\nauth=Codex subscription\nconnected_providers=Codex\nauth_refreshable=true\npermission_mode=auto\n"
            )),
            "{status}"
        );
    }

    fn activities(events: &[UiEvent]) -> Vec<CompactionActivity> {
        events
            .iter()
            .filter_map(|event| match event {
                UiEvent::CompactionActivity { activity } => Some(*activity),
                _ => None,
            })
            .collect()
    }

    fn compaction_settled(event: &UiEvent) -> bool {
        matches!(
            event,
            UiEvent::CompactionActivity {
                activity: CompactionActivity::Compacted | CompactionActivity::Ended(_)
            }
        )
    }

    async fn chat(harness: &mut Harness, prompts: &[&str]) {
        for prompt in prompts {
            harness.submit(prompt);
            harness.until(finished(TurnOutcome::Completed)).await;
        }
    }

    fn tool_work_then_chat(summary: Reply, after: &[&str]) -> FakeServer {
        let mut replies = vec![
            Reply::sse(&chat_tool_call_events(
                "call-1",
                "read_file",
                r#"{"path":"notes.md"}"#,
            )),
            Reply::sse(&chat_text_events(&["Read the notes."])),
        ];
        replies.extend(
            (1..=4).map(|turn| Reply::sse(&chat_text_events(&[&format!("answer {turn}")]))),
        );
        replies.push(summary);
        replies.extend(
            after
                .iter()
                .map(|text| Reply::sse(&chat_text_events(&[text]))),
        );
        FakeServer::start(replies)
    }

    fn file_evidence_messages(body: &Value) -> usize {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| {
                message["role"] == "user"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.starts_with("Session file evidence"))
            })
            .count()
    }

    fn first_user_message(body: &Value) -> &str {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
            .unwrap()
    }

    fn held_summary() -> Reply {
        Reply::held_sse(&chat_text_events(&["Turn 1\n"])[..1])
    }

    async fn summary_requested(server: &FakeServer) {
        timeout(Duration::from_secs(10), async {
            while server.requests().len() < 7 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the summary request reaches the provider");
        let body = server.requests()[6].json();
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("You write compaction notes"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn compact_reports_when_there_is_nothing_to_compact() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
        let mut harness = Harness::start(&server).await;
        harness.command("/compact");
        let empty = harness.until(compaction_settled).await;
        assert_eq!(
            activities(empty),
            [CompactionActivity::Ended(CompactionEnd::NothingToCompact)]
        );
        chat(&mut harness, &["first"]).await;
        harness.command("/compact");
        let fits = harness.until(compaction_settled).await;
        assert_eq!(
            activities(fits),
            [
                CompactionActivity::Preparing,
                CompactionActivity::Ended(CompactionEnd::NothingToCompact)
            ]
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn compact_replaces_older_turns_with_a_checkpoint_for_the_next_request() {
        let server = FakeServer::start(
            (1..=6).map(|turn| Reply::sse(&chat_text_events(&[&format!("answer {turn}")]))),
        );
        let mut harness = Harness::start(&server).await;
        chat(&mut harness, &["one", "two", "three", "four", "five"]).await;
        harness.command("/compact");
        let compacted = harness.until(compaction_settled).await;
        assert_eq!(
            activities(compacted),
            [
                CompactionActivity::Preparing,
                CompactionActivity::Summarizing,
                CompactionActivity::Compacted
            ]
        );
        let status = status_notice(&mut harness).await;
        assert!(status.contains("\nhistory_turns=5\n"), "{status}");
        let copied = copy_notice(&mut harness).await;
        assert_eq!(copied.body, "Copied to clipboard.");
        assert_eq!(harness.clipboard.copied(), ["answer 5"]);
        chat(&mut harness, &["six"]).await;
        let body = server.requests()[5].json();
        let checkpoint = first_user_message(&body);
        assert!(
            checkpoint.starts_with("<compacted_conversation>\n"),
            "{checkpoint}"
        );
        assert!(checkpoint.contains("one"), "{checkpoint}");
        assert_eq!(user_messages(&body), 1 + 4 + 1);
    }

    #[tokio::test]
    async fn compact_during_a_turn_asks_to_wait_for_it_once_there_is_context() {
        let held = || Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([held(), held()]);
        let mut harness = Harness::start(&server).await;
        for (prompt, end) in [
            ("first", CompactionEnd::NothingToCompact),
            ("second", CompactionEnd::Busy),
        ] {
            harness.submit(prompt);
            harness
                .until(|event| matches!(event, UiEvent::AssistantText { .. }))
                .await;
            harness.command("/compact");
            let settled = harness.until(compaction_settled).await;
            assert_eq!(activities(settled), [CompactionActivity::Ended(end)]);
            let turn_id = harness.running_turn();
            harness.send(UiCommand::Cancel { turn_id });
            harness.until(finished(TurnOutcome::Interrupted)).await;
        }
    }

    #[tokio::test]
    async fn a_cancelled_compaction_keeps_the_history_and_runs_the_prompts_sent_meanwhile() {
        let server = tool_work_then_chat(held_summary(), &["after"]);
        let mut harness = Harness::start(&server).await;
        chat(&mut harness, &["read the notes", "q1", "q2", "q3", "q4"]).await;
        harness.command("/compact");
        let summarizing = harness
            .until(|event| {
                matches!(
                    event,
                    UiEvent::CompactionActivity {
                        activity: CompactionActivity::Summarizing
                    }
                )
            })
            .await;
        assert_eq!(
            activities(summarizing),
            [
                CompactionActivity::Preparing,
                CompactionActivity::Summarizing
            ]
        );
        summary_requested(&server).await;
        harness.submit("queued");
        harness.command("/compact");
        harness.send(UiCommand::CancelCompaction);
        let cancelled = harness.until(compaction_settled).await;
        assert_eq!(
            activities(cancelled),
            [CompactionActivity::Ended(CompactionEnd::Cancelled)]
        );
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = server.requests();
        assert_eq!(requests.len(), 8);
        let body = requests[7].json();
        assert_eq!(user_messages(&body), 7);
        assert_eq!(file_evidence_messages(&body), 1);
        assert_eq!(first_user_message(&body), "read the notes");
        let sent_meanwhile = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
            .unwrap();
        assert!(
            sent_meanwhile.starts_with("<user_steering>\n")
                && sent_meanwhile.ends_with("\n\nqueued\n</user_steering>"),
            "{sent_meanwhile}"
        );
    }

    #[tokio::test]
    async fn clear_during_a_compaction_cancels_it_and_starts_over() {
        let server = tool_work_then_chat(held_summary(), &["fresh"]);
        let mut harness = Harness::start(&server).await;
        chat(&mut harness, &["read the notes", "q1", "q2", "q3", "q4"]).await;
        harness.command("/compact");
        harness
            .until(|event| {
                matches!(
                    event,
                    UiEvent::CompactionActivity {
                        activity: CompactionActivity::Summarizing
                    }
                )
            })
            .await;
        summary_requested(&server).await;
        harness.command("/clear");
        let cleared = harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        assert_eq!(
            activities(cleared),
            [CompactionActivity::Ended(CompactionEnd::Cancelled)]
        );
        chat(&mut harness, &["start over"]).await;
        assert_eq!(user_messages(&server.requests()[7].json()), 1);
    }

    #[tokio::test]
    async fn a_failed_summary_reports_the_failure_and_keeps_the_history() {
        let failure = Reply::status(400, r#"{"error":{"message":"summary rejected"}}"#);
        let server = tool_work_then_chat(failure, &["after"]);
        let mut harness = Harness::start(&server).await;
        chat(&mut harness, &["read the notes", "q1", "q2", "q3", "q4"]).await;
        harness.command("/compact");
        let failed = harness.until(compaction_settled).await;
        assert_eq!(
            activities(failed).last(),
            Some(&CompactionActivity::Ended(CompactionEnd::Failed))
        );
        chat(&mut harness, &["after"]).await;
        let body = server.requests()[7].json();
        assert_eq!(user_messages(&body), 7);
        assert_eq!(file_evidence_messages(&body), 1);
    }

    async fn fast_notice(harness: &mut Harness) -> String {
        harness.command("/fast");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { notice } if notice.topic == "fast"))
            .await;
        notice_body(shown).pop().unwrap()
    }

    async fn ultrafast_notice(harness: &mut Harness, command: &str) -> (NoticeTone, String) {
        harness.command(command);
        let shown = harness
            .until(
                |event| matches!(event, UiEvent::Notice { notice } if notice.topic == "ultrafast"),
            )
            .await;
        match shown.last() {
            Some(UiEvent::Notice { notice }) => (notice.tone, notice.body.clone()),
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn ultra_mode_requests_are_reported_and_need_a_model_that_has_it() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        for (command, tone, body) in [
            ("/ultrafast", NoticeTone::Neutral, "requested: off"),
            (
                "/ultrafast ON",
                NoticeTone::Warning,
                "Ultra mode is unavailable for this model and may increase cost.",
            ),
            ("/ultrafast  status ", NoticeTone::Neutral, "requested: off"),
            ("/ultrafast off", NoticeTone::Neutral, "requested off"),
        ] {
            assert_eq!(
                ultrafast_notice(&mut harness, command).await,
                (tone, body.to_owned()),
                "{command}"
            );
        }
        harness.command("/ultrafast faster");
        assert_eq!(
            notices_until(&mut harness, "").await,
            ["|usage: /ultrafast [on|off|status]"]
        );
    }

    async fn leave_a_session_saved_with_ultra(
        harness: &mut Harness,
    ) -> (String, std::path::PathBuf) {
        chat(harness, &["first question"]).await;
        let id = saved_sessions(&harness.home)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let manifest = harness
            .home
            .path()
            .join("data/sessions")
            .join(&id)
            .join("session.json");
        start_new_session(harness).await;
        let saved = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            saved.replace(
                "\"fast_mode\":false,",
                "\"fast_mode\":false,\"ultrafast_mode\":true,",
            ),
        )
        .unwrap();
        (id, manifest)
    }

    async fn start_new_session(harness: &mut Harness) {
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
    }

    async fn resume_session(harness: &mut Harness, id: &str) {
        harness.send(UiCommand::ResumeSession { id: id.to_owned() });
        harness
            .until(|event| matches!(event, UiEvent::SessionResumed { .. }))
            .await;
    }

    #[tokio::test]
    async fn a_saved_ultra_request_comes_back_with_its_session_and_turning_it_off_is_saved() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
        let mut harness = Harness::start_saved(&server).await;
        let (id, manifest) = leave_a_session_saved_with_ultra(&mut harness).await;
        resume_session(&mut harness, &id).await;
        let requested = |on: &str| (NoticeTone::Neutral, format!("requested: {on}"));
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast").await,
            requested("on")
        );
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast off").await,
            (NoticeTone::Neutral, "requested off".to_owned())
        );
        assert!(
            !fs::read_to_string(&manifest)
                .unwrap()
                .contains("ultrafast_mode")
        );
        start_new_session(&mut harness).await;
        resume_session(&mut harness, &id).await;
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast").await,
            requested("off")
        );
    }

    #[tokio::test]
    async fn turning_ultra_off_while_it_is_off_keeps_a_resumed_sessions_saved_request_off() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
        let mut harness = Harness::start_saved(&server).await;
        let (id, manifest) = leave_a_session_saved_with_ultra(&mut harness).await;
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast off").await,
            (NoticeTone::Neutral, "requested off".to_owned())
        );
        resume_session(&mut harness, &id).await;
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast").await,
            (NoticeTone::Neutral, "requested: off".to_owned())
        );
        assert!(
            fs::read_to_string(&manifest)
                .unwrap()
                .contains("\"ultrafast_mode\":true")
        );
    }

    #[tokio::test]
    async fn an_ultra_request_from_launch_lasts_until_turned_off_or_the_model_changes() {
        let server = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let setup = agent_setup(&home, &server).await;
        let mut harness = Harness::requesting_ultrafast(home, setup);
        let requested = (NoticeTone::Neutral, "requested: on".to_owned());
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast").await,
            requested
        );
        let status = status_notice(&mut harness).await;
        assert!(status.ends_with("\nultrafast_requested=true"), "{status}");
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast off").await,
            (NoticeTone::Neutral, "requested off".to_owned())
        );
        let status = status_notice(&mut harness).await;
        assert!(status.ends_with("\nultrafast_requested=false"), "{status}");

        let home = tempfile::tempdir().unwrap();
        let setup = agent_setup(&home, &server).await;
        let mut switched = Harness::requesting_ultrafast(home, setup);
        switched.command("/model model-a");
        notices_until(&mut switched, "").await;
        assert_eq!(
            ultrafast_notice(&mut switched, "/ultrafast").await,
            requested
        );
        switched.command("/model model-b");
        notices_until(&mut switched, "").await;
        assert_eq!(
            ultrafast_notice(&mut switched, "/ultrafast").await,
            (NoticeTone::Neutral, "requested: off".to_owned())
        );
    }

    #[tokio::test]
    async fn turning_fast_mode_on_withdraws_the_ultra_request() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(true, 8);
        let home = codex_home();
        let setup =
            agent_setup_with(&home, &codex_settings(), codex_endpoints(&codex, &catalog)).await;
        let mut harness = Harness::requesting_ultrafast(home, setup);
        assert_eq!(fast_notice(&mut harness).await, "fast|on");
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast").await,
            (NoticeTone::Neutral, "requested: off".to_owned())
        );
    }

    #[tokio::test]
    async fn fast_mode_needs_a_model_that_comes_with_it() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        assert_eq!(
            fast_notice(&mut harness).await,
            "fast|This model does not come with a fast mode."
        );
        assert_eq!(
            fast_notice(&mut harness).await,
            "fast|This model does not come with a fast mode."
        );
    }

    #[tokio::test]
    async fn fast_mode_toggles_the_priority_tier_of_the_next_codex_requests() {
        let codex = FakeServer::start([codex_text("fast"), codex_text("standard")]);
        let catalog = codex_catalog(true, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        assert_eq!(fast_notice(&mut harness).await, "fast|on");
        harness.submit("hurry");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(fast_notice(&mut harness).await, "fast|off");
        harness.submit("relax");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = codex.requests();
        assert_eq!(requests[0].json()["service_tier"], "priority");
        assert_eq!(requests[1].json().get("service_tier"), None);
    }

    #[tokio::test]
    async fn the_codex_catalog_is_fetched_once_for_every_later_fast_check() {
        let codex = FakeServer::start([codex_text("fast")]);
        let catalog = codex_catalog(true, 1);
        let mut harness = Harness::codex(&codex, &catalog).await;
        for expected in ["fast|on", "fast|off", "fast|on"] {
            assert_eq!(fast_notice(&mut harness).await, expected);
        }
        harness.command(&format!("/model {OTHER_CODEX_MODEL}"));
        assert_eq!(fast_notice(&mut harness).await, "fast|on");
        harness.submit("hurry");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(codex.requests()[0].json()["service_tier"], "priority");
        assert_eq!(catalog.requests().len(), 2);
    }

    #[tokio::test]
    async fn fast_mode_enabled_after_a_failed_catalog_lookup_reaches_the_next_request() {
        let codex = FakeServer::start([codex_text("one"), codex_text("two"), codex_text("three")]);
        let catalog = FakeServer::start([
            catalog_version(),
            Reply::status(400, "{}"),
            catalog_listing(true),
        ]);
        let mut harness = Harness::codex(&codex, &catalog).await;
        chat(&mut harness, &["one", "two"]).await;
        assert_eq!(catalog.requests().len(), 2);
        assert_eq!(fast_notice(&mut harness).await, "fast|on");
        chat(&mut harness, &["three"]).await;
        let requests = codex.requests();
        assert_eq!(requests[1].json().get("service_tier"), None);
        assert_eq!(requests[2].json()["service_tier"], "priority");
        assert_eq!(catalog.requests().len(), 3);
    }

    #[tokio::test]
    async fn a_ready_upgrade_keeps_the_new_session_for_the_relaunch_and_stops_serving() {
        use crate::app_upgrade_runtime::{Readiness, Relaunch, UpgradeState};

        let server = FakeServer::start(Vec::new());
        let relaunch = Relaunch::default();
        let upgrade = UpgradeShortcut::new(
            Some(Readiness::settled(UpgradeState::Ready)),
            relaunch.clone(),
        );
        let mut harness = Harness::upgrading(&server, upgrade, None).await;
        harness.send(UiCommand::ApplyReadyUpgrade);
        harness
            .until(|event| *event == UiEvent::ExitRequested)
            .await;
        timeout(Duration::from_secs(10), harness.until(|_| false))
            .await
            .expect("the controller stops after requesting the relaunch");
        let sessions = saved_sessions(&harness.home);
        assert_eq!(sessions.len(), 1);
        let mut argv = Vec::new();
        let _ = relaunch.run_with(|command| {
            argv.push(command.get_program().to_owned());
            argv.extend(command.get_args().map(ToOwned::to_owned));
            std::io::Error::from(std::io::ErrorKind::NotFound)
        });
        assert_eq!(
            argv,
            [
                ofx_upgrade::installed_executable()
                    .unwrap()
                    .into_os_string(),
                "resume".into(),
                sessions[0]["id"].as_str().unwrap().into(),
                "--upgrade-relaunch".into(),
            ]
        );
    }

    #[tokio::test]
    async fn a_ready_upgrade_relaunches_with_the_launchs_ultra_request() {
        use crate::app_upgrade_runtime::{Readiness, Relaunch, UpgradeState};

        let server = FakeServer::start(Vec::new());
        let relaunch = Relaunch::default();
        let upgrade = UpgradeShortcut::new(
            Some(Readiness::settled(UpgradeState::Ready)),
            relaunch.clone(),
        );
        let mut harness = Harness::upgrading(&server, upgrade, Some(true)).await;
        let argv = apply_ready_upgrade(&mut harness, &relaunch).await;
        assert_eq!(argv[0], "--ultrafast");
        assert_eq!(argv[1], "resume");
    }

    #[tokio::test]
    async fn a_ready_upgrade_relaunch_keeps_an_off_choice_over_a_saved_ultra_request() {
        use crate::app_upgrade_runtime::{Readiness, Relaunch, UpgradeState};

        let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
        let relaunch = Relaunch::default();
        let upgrade = UpgradeShortcut::new(
            Some(Readiness::settled(UpgradeState::Ready)),
            relaunch.clone(),
        );
        let mut harness = Harness::upgrading(&server, upgrade, Some(false)).await;
        let (id, _) = leave_a_session_saved_with_ultra(&mut harness).await;
        resume_session(&mut harness, &id).await;
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast").await,
            (NoticeTone::Neutral, "requested: off".to_owned())
        );
        let argv = apply_ready_upgrade(&mut harness, &relaunch).await;
        assert_eq!(
            argv,
            [
                "--no-ultrafast",
                "resume",
                id.as_str(),
                "--upgrade-relaunch"
            ]
        );
        let Ok(ofx_cli::Invocation::Resume(relaunched, _)) = ofx_cli::parse_args(argv) else {
            panic!("the relaunch resumes the session");
        };
        assert_eq!(relaunched.ultrafast_mode(), Some(false));
    }

    async fn apply_ready_upgrade(
        harness: &mut Harness,
        relaunch: &crate::app_upgrade_runtime::Relaunch,
    ) -> Vec<std::ffi::OsString> {
        harness.send(UiCommand::ApplyReadyUpgrade);
        harness
            .until(|event| *event == UiEvent::ExitRequested)
            .await;
        timeout(Duration::from_secs(10), harness.until(|_| false))
            .await
            .expect("the controller stops after requesting the relaunch");
        let mut argv = Vec::new();
        let _ = relaunch.run_with(|command| {
            argv.extend(command.get_args().map(ToOwned::to_owned));
            std::io::Error::from(std::io::ErrorKind::NotFound)
        });
        argv
    }

    fn saved_sessions(home: &tempfile::TempDir) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(home.path().join("data/sessions")) else {
            return Vec::new();
        };
        entries
            .map(|entry| {
                let manifest = entry.unwrap().path().join("session.json");
                serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap()
            })
            .collect()
    }

    fn titled(title: Option<&str>) -> impl Fn(&UiEvent) -> bool {
        move |event| matches!(event, UiEvent::SessionTitleChanged { title: seen } if seen.as_deref() == title)
    }

    async fn until_titled(harness: &mut Harness, title: &str) {
        if !harness.seen.iter().any(titled(Some(title))) {
            harness.until(titled(Some(title))).await;
        }
    }

    fn title_changes(events: &[UiEvent]) -> Vec<Option<String>> {
        events
            .iter()
            .filter_map(|event| match event {
                UiEvent::SessionTitleChanged { title } => Some(title.clone()),
                _ => None,
            })
            .collect()
    }

    fn title_requests(codex: &FakeServer) -> Vec<RecordedRequest> {
        codex
            .requests()
            .into_iter()
            .filter(|request| request.json()["model"] == CODEX_TITLE_MODEL)
            .collect()
    }

    #[tokio::test]
    async fn the_first_prompt_of_a_fresh_codex_session_names_it_in_the_background() {
        let codex = FakeServer::start([
            codex_text("Fix the renderer"),
            codex_text("Fix the renderer"),
            codex_text("done"),
        ]);
        let catalog = codex_catalog(false, 8);
        let mut harness = Harness::codex_saved(&codex, &catalog, &codex_settings()).await;
        chat(&mut harness, &["  please fix the renderer\n"]).await;
        until_titled(&mut harness, "Fix the renderer").await;
        assert_eq!(
            saved_sessions(&harness.home)[0]["title"],
            "Fix the renderer"
        );
        chat(&mut harness, &["now the tests"]).await;
        let changes = title_changes(&harness.seen);
        assert_eq!(changes.last(), Some(&Some("Fix the renderer".to_owned())));
        assert!(changes.len() <= 2, "{changes:?}");
        let titles = title_requests(&codex);
        assert_eq!(titles.len(), 1);
        let body = titles[0].json();
        assert!(
            body["instructions"]
                .as_str()
                .unwrap()
                .starts_with("Generate a short title for a conversation"),
            "{body}"
        );
        assert_eq!(body["input"].as_array().unwrap().len(), 1, "{body}");
        assert!(
            body["input"]
                .to_string()
                .contains("please fix the renderer"),
            "{body}"
        );
        assert_eq!(body["tool_choice"], "none");
        assert_eq!(body.get("tools"), None);
        let session = &saved_sessions(&harness.home)[0];
        assert_eq!(titles[0].header("session-id"), session["id"].as_str());
        assert_eq!(codex.requests().len(), 3);
    }

    async fn rename_notice(harness: &mut Harness, command: &str) -> String {
        harness.command(command);
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        notice_body(shown).join("\n")
    }

    fn rename_tone(harness: &Harness) -> NoticeTone {
        harness
            .seen
            .iter()
            .rev()
            .find_map(|event| match event {
                UiEvent::Notice { notice } => Some(notice.tone),
                _ => None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn rename_validates_the_title_and_saves_it_in_the_session() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
        let mut harness = Harness::start_saved(&server).await;
        assert_eq!(
            rename_notice(&mut harness, "/rename").await,
            "|usage: /rename <title>"
        );
        assert_eq!(rename_tone(&harness), NoticeTone::Error);
        let too_long = format!("/rename {}", "x".repeat(241));
        assert_eq!(
            rename_notice(&mut harness, &too_long).await,
            "session|title is too long"
        );
        assert_eq!(
            rename_notice(&mut harness, "/rename bad\x07title").await,
            "session|title must be printable text"
        );
        assert_eq!(title_changes(&harness.seen), []);
        assert_eq!(
            rename_notice(&mut harness, "/rename   deploy pipeline fix ").await,
            "session|renamed to \"deploy pipeline fix\""
        );
        assert_eq!(rename_tone(&harness), NoticeTone::Neutral);
        assert_eq!(
            saved_sessions(&harness.home)[0]["title"],
            "deploy pipeline fix"
        );
        chat(&mut harness, &["ship it"]).await;
        assert_eq!(
            saved_sessions(&harness.home)[0]["title"],
            "deploy pipeline fix"
        );
        assert_eq!(
            title_changes(&harness.seen),
            [Some("deploy pipeline fix".to_owned())]
        );
    }

    #[tokio::test]
    async fn a_session_renamed_before_its_first_prompt_is_kept_and_never_generated() {
        let codex = FakeServer::start([codex_text("done")]);
        let catalog = codex_catalog(false, 8);
        let mut harness = Harness::codex_saved(&codex, &catalog, &codex_settings()).await;
        assert_eq!(
            rename_notice(&mut harness, "/rename Release prep").await,
            "session|renamed to \"Release prep\""
        );
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        let mut titles: Vec<Value> = saved_sessions(&harness.home)
            .iter()
            .map(|session| session["title"].clone())
            .collect();
        titles.sort_by_key(Value::is_string);
        assert_eq!(titles, [Value::Null, json!("Release prep")]);
        assert_eq!(
            rename_notice(&mut harness, "/rename Second").await,
            "session|renamed to \"Second\""
        );
        chat(&mut harness, &["fix the renderer"]).await;
        assert_eq!(codex.requests().len(), 1);
        assert!(title_requests(&codex).is_empty());
    }

    #[tokio::test]
    async fn a_session_resumed_from_the_picker_names_the_window() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
        let mut harness = Harness::start_saved(&server).await;
        chat(&mut harness, &["first question"]).await;
        assert_eq!(
            rename_notice(&mut harness, "/rename Release prep").await,
            "session|renamed to \"Release prep\""
        );
        let named = saved_sessions(&harness.home)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        let start = harness.seen.len();
        harness.send(UiCommand::ResumeSession { id: named });
        harness
            .until(|event| matches!(event, UiEvent::SessionResumed { .. }))
            .await;
        assert_eq!(
            title_changes(&harness.seen[start..]),
            [Some("Release prep".to_owned())]
        );
    }

    #[tokio::test]
    async fn the_picker_lists_saved_sessions_from_a_background_scan_shared_by_both_scopes() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
        let mut harness = Harness::start_saved(&server).await;
        chat(&mut harness, &["first question"]).await;
        let saved = saved_sessions(&harness.home)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        for scope in [SessionScope::CurrentWorkspace, SessionScope::AllWorkspaces] {
            harness.send(UiCommand::ListSessions {
                scope,
                after: None,
                limit: 10,
            });
        }
        let mut listed = Vec::new();
        while listed.len() < 2 {
            let events = harness
                .until(|event| matches!(event, UiEvent::SessionsListed { .. }))
                .await;
            if let Some(UiEvent::SessionsListed { page }) = events.last() {
                let ids: Vec<String> = page.rows.iter().map(|row| row.id.clone()).collect();
                listed.push((page.scope, ids));
            }
        }
        assert_eq!(
            listed,
            [
                (SessionScope::CurrentWorkspace, vec![saved.clone()]),
                (SessionScope::AllWorkspaces, vec![saved]),
            ]
        );
    }

    #[tokio::test]
    async fn a_rename_during_a_turn_applies_at_once() {
        let gate = Gate::default();
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"])).after(&gate)]);
        let mut harness = Harness::start_saved(&server).await;
        harness.submit("work");
        harness
            .until(|event| matches!(event, UiEvent::TurnStarted { .. }))
            .await;
        assert_eq!(
            rename_notice(&mut harness, "/rename Busy work").await,
            "session|renamed to \"Busy work\""
        );
        gate.open();
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(saved_sessions(&harness.home)[0]["title"], "Busy work");
    }

    #[tokio::test]
    async fn rename_needs_a_saved_session() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        assert_eq!(
            rename_notice(&mut harness, "/rename Release prep").await,
            "session|no active session to rename"
        );
        assert_eq!(
            rename_notice(&mut harness, "/rename  ").await,
            "|usage: /rename <title>"
        );
    }

    #[tokio::test]
    async fn the_first_prompt_titles_the_session_until_a_fresh_one_starts() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["one"])),
            Reply::sse(&chat_text_events(&["two"])),
        ]);
        let mut harness = Harness::start(&server).await;
        chat(&mut harness, &["  fix the flaky\x07 test\nmore", "again"]).await;
        harness.command("/clear");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        assert_eq!(
            title_changes(&harness.seen),
            [Some("fix the flaky test".to_owned()), None]
        );
    }

    #[tokio::test]
    async fn a_fresh_session_is_named_while_the_previous_title_request_still_waits() {
        let held = Gate::default();
        let codex = FakeServer::start([
            codex_text("First").after(&held),
            codex_text("First").after(&held),
            codex_text("Second title"),
            codex_text("Second title"),
        ]);
        let catalog = codex_catalog(false, 8);
        let mut harness = Harness::codex_saved(&codex, &catalog, &codex_settings()).await;
        harness.submit("fix the renderer");
        harness
            .until(|event| matches!(event, UiEvent::TurnStarted { .. }))
            .await;
        while codex.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        harness.submit("now the tests");
        timeout(
            Duration::from_secs(10),
            until_titled(&mut harness, "Second title"),
        )
        .await
        .expect("the fresh session is named");
        let titles = title_requests(&codex);
        assert_eq!(titles.len(), 2);
        let fresh = saved_sessions(&harness.home)
            .into_iter()
            .find(|session| session["title"] == "Second title")
            .expect("the fresh session keeps its generated title");
        assert_eq!(titles[1].header("session-id"), fresh["id"].as_str());
        assert_ne!(titles[0].header("session-id"), fresh["id"].as_str());
    }

    #[tokio::test]
    async fn sessions_keep_their_first_prompt_title_when_session_titles_are_off() {
        let codex = FakeServer::start([codex_text("done")]);
        let catalog = codex_catalog(false, 8);
        let settings = json!({
            "provider": "codex",
            "models": {"codex": CODEX_MODEL},
            "session_titles": false
        });
        let mut harness = Harness::codex_saved(&codex, &catalog, &settings).await;
        chat(&mut harness, &["please fix the renderer"]).await;
        assert_eq!(codex.requests().len(), 1);
        assert_eq!(
            saved_sessions(&harness.home)[0]["title"],
            "please fix the renderer"
        );
    }

    #[tokio::test]
    async fn fast_mode_stays_off_for_a_codex_model_without_a_fast_tier() {
        let codex = FakeServer::start([codex_text("standard")]);
        let catalog = codex_catalog(false, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        assert_eq!(
            fast_notice(&mut harness).await,
            "fast|This model does not come with a fast mode."
        );
        harness.submit("go");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(codex.requests()[0].json().get("service_tier"), None);
    }

    #[tokio::test]
    async fn fast_mode_switched_during_a_turn_applies_to_the_next_one() {
        let codex = FakeServer::start([codex_partial(), codex_text("next")]);
        let catalog = codex_catalog(true, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        assert_eq!(fast_notice(&mut harness).await, "fast|on");
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
        harness.submit("next");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = codex.requests();
        assert_eq!(requests[0].json().get("service_tier"), None);
        assert_eq!(requests[1].json()["service_tier"], "priority");
    }

    #[tokio::test]
    async fn another_model_starts_without_fast_mode_and_the_same_one_keeps_it() {
        let codex = FakeServer::start([codex_text("same"), codex_text("other")]);
        let catalog = codex_catalog(true, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        assert_eq!(fast_notice(&mut harness).await, "fast|on");
        harness.command(&format!("/model {CODEX_MODEL}"));
        harness.submit("same");
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.command(&format!("/model {OTHER_CODEX_MODEL}"));
        harness.submit("other");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = codex.requests();
        assert_eq!(requests[0].json()["service_tier"], "priority");
        assert_eq!(requests[1].json()["model"], OTHER_CODEX_MODEL);
        assert_eq!(requests[1].json().get("service_tier"), None);
        assert_eq!(fast_notice(&mut harness).await, "fast|on");
    }

    fn settings_opened(event: &UiEvent) -> bool {
        matches!(event, UiEvent::SettingsMenuOpened { .. })
    }

    fn settings_changed(event: &UiEvent) -> bool {
        matches!(event, UiEvent::SettingsChanged { .. })
    }

    fn last_settings(events: &[UiEvent]) -> SettingsSnapshot {
        events
            .iter()
            .rev()
            .find_map(|event| match event {
                UiEvent::SettingsMenuOpened { snapshot }
                | UiEvent::SettingsChanged { snapshot } => Some(snapshot.clone()),
                _ => None,
            })
            .unwrap()
    }

    async fn step_setting(harness: &mut Harness, setting: SettingId, delta: isize) -> Vec<UiEvent> {
        harness.send(UiCommand::StepSetting { setting, delta });
        harness.until(settings_changed).await.to_vec()
    }

    #[tokio::test]
    async fn a_bare_settings_opens_the_menu_with_the_current_settings() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        harness.command("/settings");
        let shown = harness.until(settings_opened).await;
        assert_eq!(notice_body(shown), Vec::<String>::new());
        assert_eq!(
            last_settings(shown),
            SettingsSnapshot {
                model: "model-a".to_owned(),
                effort: "default".to_owned(),
                reasoning_efforts: Vec::new(),
                fast_mode: FastModeSetting::Unavailable,
                permission_mode: PermissionMode::Auto,
                statusline: StatuslineToggles::default(),
                session_titles: true,
                startup_scrollback: true,
                prompt_history: true,
            }
        );
    }

    #[tokio::test]
    async fn a_bare_settings_reports_settings_it_cannot_load_instead_of_opening() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        fs::write(
            harness.home.path().join("config/settings.json"),
            json!({"providers": 5}).to_string(),
        )
        .unwrap();
        harness.command("/settings");
        let shown = harness
            .until(|event| settings_opened(event) || matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(
            notice_body(shown),
            ["settings|Failed to load settings: InvalidObject"]
        );
    }

    #[tokio::test]
    async fn settings_menu_changes_apply_and_save_as_their_commands_do() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        harness.command("/settings");
        harness.until(settings_opened).await;
        let shown = step_setting(&mut harness, SettingId::PermissionMode, -1).await;
        assert_eq!(notice_body(&shown), ["permissions|mode set to ask"]);
        assert_eq!(last_settings(&shown).permission_mode, PermissionMode::Ask);
        let shown = step_setting(&mut harness, SettingId::StatuslineWorkspace, 1).await;
        assert!(shown.contains(&UiEvent::StatuslineChanged {
            item: StatuslineItem::Workspace,
            enabled: true,
        }));
        assert_eq!(
            notice_body(&shown),
            [
                "statusline|saved to user settings (scope=user)",
                "statusline|workspace: on",
            ]
        );
        assert!(
            last_settings(&shown)
                .statusline
                .enabled(StatuslineItem::Workspace)
        );
        let shown = step_setting(&mut harness, SettingId::SessionTitles, 1).await;
        assert_eq!(
            notice_body(&shown),
            ["session titles|saved to user settings (scope=user)"]
        );
        assert!(!last_settings(&shown).session_titles);
        let shown = step_setting(&mut harness, SettingId::StartupScrollback, 1).await;
        assert_eq!(
            notice_body(&shown),
            ["settings|startup_scrollback: off (applies on next launch)"]
        );
        assert!(!last_settings(&shown).startup_scrollback);
        let shown = step_setting(&mut harness, SettingId::PromptHistory, 1).await;
        assert!(shown.contains(&UiEvent::PromptHistoryChanged { enabled: false }));
        assert_eq!(
            notice_body(&shown),
            ["history|saved to user settings (scope=user)"]
        );
        assert!(!last_settings(&shown).prompt_history);
        let shown = step_setting(&mut harness, SettingId::FastMode, 1).await;
        assert_eq!(
            notice_body(&shown),
            ["fast|This model does not come with a fast mode."]
        );
        assert_eq!(
            last_settings(&shown).fast_mode,
            FastModeSetting::Unavailable
        );
        let saved = saved_settings(&harness);
        assert_eq!(saved["permission_mode"], "ask");
        assert_eq!(saved["statusLine"], json!({"workspace": true}));
        assert_eq!(saved["session_titles"], false);
        assert_eq!(saved["startup_scrollback"], false);
        assert_eq!(saved["prompt_history"], json!({"enabled": false}));
    }

    #[tokio::test]
    async fn steps_sent_before_a_reply_each_move_the_value_the_controller_holds() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        harness.command("/settings");
        harness.until(settings_opened).await;
        for _ in 0..2 {
            harness.send(UiCommand::StepSetting {
                setting: SettingId::StatuslineWorkspace,
                delta: 1,
            });
        }
        let mut shown = Vec::new();
        for _ in 0..2 {
            shown.extend(harness.until(settings_changed).await.iter().cloned());
        }
        let workspace: Vec<bool> = shown
            .iter()
            .filter_map(|event| match event {
                UiEvent::SettingsChanged { snapshot } => {
                    Some(snapshot.statusline.enabled(StatuslineItem::Workspace))
                }
                _ => None,
            })
            .collect();
        assert_eq!(workspace, [true, false]);
        assert_eq!(
            saved_settings(&harness)["statusLine"],
            json!({"workspace": false})
        );
    }

    #[tokio::test]
    async fn prompt_history_switched_off_stops_recording_before_its_save_waits() {
        let server = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let setup = agent_setup(&home, &server).await;
        let config = PrivateDir::open_existing(&home.path().join("config"))
            .unwrap()
            .unwrap();
        let held = Arc::new(Mutex::new(config.try_lock("settings.lock").unwrap()));
        let release = Arc::clone(&held);
        let mut harness = Harness::with_setup_observer(home, setup, move |event| {
            if matches!(event, UiEvent::PromptHistoryChanged { enabled: false }) {
                release.lock().unwrap().take();
            }
        });
        harness.command("/settings");
        harness.until(settings_opened).await;
        harness.send(UiCommand::StepSetting {
            setting: SettingId::PromptHistory,
            delta: 1,
        });
        let shown = harness.until(settings_changed).await;
        assert_eq!(
            notice_body(shown),
            ["history|saved to user settings (scope=user)"]
        );
        assert!(held.lock().unwrap().is_none());
        assert_eq!(
            saved_settings(&harness)["prompt_history"],
            json!({"enabled": false})
        );
    }

    #[tokio::test]
    async fn the_effort_row_steps_through_the_model_s_efforts_and_saves_the_choice() {
        let codex = FakeServer::start([codex_text("thought")]);
        let catalog = codex_catalog(false, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        harness.command("/settings");
        let opened = last_settings(harness.until(settings_opened).await);
        assert_eq!(opened.effort, "default");
        assert_eq!(opened.reasoning_efforts, ["low"]);
        let shown = step_setting(&mut harness, SettingId::Effort, 1).await;
        assert_eq!(notice_body(&shown), Vec::<String>::new());
        assert_eq!(last_settings(&shown).effort, "low");
        harness.submit("think");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(codex.requests()[0].json()["reasoning"]["effort"], "low");
        assert_eq!(saved_settings(&harness)["effort"], "low");
        let shown = step_setting(&mut harness, SettingId::Effort, 1).await;
        assert_eq!(last_settings(&shown).effort, "default");
        assert_eq!(saved_settings(&harness)["effort"], "auto");
    }

    #[tokio::test]
    async fn a_model_picked_from_the_settings_menu_keeps_the_effort_and_fast_mode() {
        let codex = FakeServer::start([codex_text("picked")]);
        let catalog = codex_catalog(true, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        harness.command("/settings");
        harness.until(settings_opened).await;
        step_setting(&mut harness, SettingId::Effort, 1).await;
        step_setting(&mut harness, SettingId::FastMode, 1).await;
        harness.send(UiCommand::SelectModelFromSettings {
            model: OTHER_CODEX_MODEL.to_owned(),
        });
        let shown = harness.until(settings_changed).await.to_vec();
        assert_eq!(
            notice_body(&shown),
            [format!("|Switched to {OTHER_CODEX_MODEL}")]
        );
        assert!(shown.contains(&UiEvent::ModelSelected {
            model: OTHER_CODEX_MODEL.to_owned(),
        }));
        let snapshot = last_settings(&shown);
        assert_eq!(snapshot.model, OTHER_CODEX_MODEL);
        assert_eq!(snapshot.effort, "low");
        assert_eq!(snapshot.fast_mode, FastModeSetting::On);
        harness.submit("go");
        harness.until(finished(TurnOutcome::Completed)).await;
        let request = codex.requests()[0].json();
        assert_eq!(request["model"], OTHER_CODEX_MODEL);
        assert_eq!(request["reasoning"]["effort"], "low");
        assert_eq!(request["service_tier"], "priority");
        let saved = saved_settings(&harness);
        assert_eq!(saved["models"]["codex"], OTHER_CODEX_MODEL);
        assert_eq!(saved["effort"], "low");
        assert_eq!(saved["fast_mode"], true);
    }

    #[tokio::test]
    async fn a_model_picked_from_the_settings_menu_during_a_turn_serves_the_next_turn() {
        let gate = Gate::default();
        let codex = FakeServer::start([codex_text("first").after(&gate), codex_text("second")]);
        let catalog = codex_catalog(false, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        harness.submit("work");
        harness
            .until(|event| matches!(event, UiEvent::TurnStarted { .. }))
            .await;
        harness.command("/settings");
        within(harness.until(settings_opened)).await;
        harness.send(UiCommand::SelectModelFromSettings {
            model: OTHER_CODEX_MODEL.to_owned(),
        });
        let shown = within(harness.until(settings_changed)).await.to_vec();
        assert_eq!(last_settings(&shown).model, OTHER_CODEX_MODEL);
        assert!(
            !shown
                .iter()
                .any(|event| matches!(event, UiEvent::TurnFinished { .. }))
        );
        gate.open();
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.submit("next");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = codex.requests();
        assert_ne!(requests[0].json()["model"], OTHER_CODEX_MODEL);
        assert_eq!(requests[1].json()["model"], OTHER_CODEX_MODEL);
    }

    #[tokio::test]
    async fn fast_mode_changed_in_the_settings_menu_reaches_the_next_request() {
        let codex = FakeServer::start([codex_text("fast")]);
        let catalog = codex_catalog(true, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        harness.command("/settings");
        let shown = harness.until(settings_opened).await;
        assert_eq!(last_settings(shown).fast_mode, FastModeSetting::Off);
        let shown = step_setting(&mut harness, SettingId::FastMode, 1).await;
        assert_eq!(notice_body(&shown), ["fast|on"]);
        assert_eq!(last_settings(&shown).fast_mode, FastModeSetting::On);
        harness.submit("hurry");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(codex.requests()[0].json()["service_tier"], "priority");
    }

    #[tokio::test]
    async fn a_settings_change_during_a_turn_applies_at_once() {
        let gate = Gate::default();
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"])).after(&gate)]);
        let mut harness = Harness::start(&server).await;
        harness.submit("work");
        harness
            .until(|event| matches!(event, UiEvent::TurnStarted { .. }))
            .await;
        harness.command("/settings");
        harness.until(settings_opened).await;
        let shown = step_setting(&mut harness, SettingId::SessionTitles, 1).await;
        assert!(!last_settings(&shown).session_titles);
        assert!(
            !shown
                .iter()
                .any(|event| matches!(event, UiEvent::TurnFinished { .. }))
        );
        gate.open();
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(saved_settings(&harness)["session_titles"], false);
    }

    #[tokio::test]
    async fn settings_opened_during_a_turn_wait_for_a_cold_catalog_to_offer_fast_mode() {
        let listed = Gate::default();
        let codex = FakeServer::start([codex_partial()]);
        let catalog = FakeServer::start([catalog_version(), catalog_listing(true).after(&listed)]);
        let mut harness = Harness::codex(&codex, &catalog).await;
        harness.submit("slow");
        within(harness.until(|event| matches!(event, UiEvent::AssistantText { .. }))).await;
        harness.command("/settings");
        listed.open();
        let shown = within(harness.until(settings_opened)).await;
        assert_eq!(last_settings(shown).fast_mode, FastModeSetting::Off);
        assert!(
            !harness
                .seen
                .iter()
                .any(|event| matches!(event, UiEvent::TurnFinished { .. }))
        );
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        within(harness.until(finished(TurnOutcome::Interrupted))).await;
    }

    fn saved_settings(harness: &Harness) -> Value {
        let text = fs::read_to_string(harness.home.path().join("config/settings.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    async fn notices_until(harness: &mut Harness, topic: &str) -> Vec<String> {
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { notice } if notice.topic == topic))
            .await;
        notice_body(shown)
    }

    fn catalog_event(event: &UiEvent) -> bool {
        matches!(event, UiEvent::ModelCatalog { .. })
    }

    async fn listed_catalog(harness: &mut Harness) -> ModelCatalog {
        harness.send(UiCommand::ListModels);
        match harness.until(catalog_event).await.last() {
            Some(UiEvent::ModelCatalog { catalog, .. }) => catalog.clone(),
            other => panic!("{other:?}"),
        }
    }

    fn select(model: &str, effort: ReasoningEffort, fast_mode: Option<bool>) -> UiCommand {
        UiCommand::SelectModel {
            model: model.to_owned(),
            effort,
            fast_mode,
        }
    }

    fn low() -> ReasoningEffort {
        ReasoningEffort::Named("low".to_owned())
    }

    #[tokio::test]
    async fn the_picker_lists_a_connection_s_models_with_their_metadata() {
        let server = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let settings = json!({
            "provider": "local",
            "providers": {"local": {
                "protocol": "openai-chat-completions",
                "base_url": server.base_url(),
                "auth": {"type": "none"},
                "models": ["model-a", "vendor/model-b"],
                "model_metadata": {"vendor/model-b": {"context_window": 128_000, "max_output_tokens": 16_000}}
            }}
        });
        let setup = agent_setup_with(&home, &settings, SubscriptionEndpoints::default()).await;
        let mut harness = Harness::with_setup(home, setup);
        let ModelCatalog::Listed { models, source } = listed_catalog(&mut harness).await else {
            panic!("the connection lists its models");
        };
        assert_eq!(source, ofx_contract::ModelCatalogSource::ProfileSettings);
        let ids: Vec<&str> = models.iter().map(|option| option.id.as_str()).collect();
        assert_eq!(ids, ["model-a", "vendor/model-b"]);
        assert_eq!(models[1].capabilities.context_window, Some(128_000));
        assert_eq!(models[1].max_output_tokens, Some(16_000));
        assert!(models[1].capabilities.reasoning_efforts.is_empty());
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn codex_catalog_failures_offer_a_retry_only_when_upstream_would() {
        for (status, retry) in [
            (503, Some(ofx_contract::CatalogRetry::Unreachable)),
            (502, Some(ofx_contract::CatalogRetry::Unreachable)),
            (429, Some(ofx_contract::CatalogRetry::RateLimited)),
            (501, None),
            (505, None),
            (403, None),
        ] {
            let codex = FakeServer::start([]);
            let catalog = FakeServer::start([catalog_version(), Reply::status(status, "{}")]);
            let mut harness = Harness::codex(&codex, &catalog).await;
            assert_eq!(
                listed_catalog(&mut harness).await,
                ModelCatalog::Failed { retry },
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn a_picked_model_withdraws_the_ultra_request() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(true, 2);
        let home = codex_home();
        let setup =
            agent_setup_with(&home, &codex_settings(), codex_endpoints(&codex, &catalog)).await;
        let mut harness = Harness::requesting_ultrafast(home, setup);
        let ModelCatalog::Listed { models, .. } = listed_catalog(&mut harness).await else {
            panic!("the Codex catalog lists its models");
        };
        harness.send(select(&models[0].id, ReasoningEffort::Auto, Some(false)));
        harness
            .until(|event| matches!(event, UiEvent::ModelSelected { .. }))
            .await;
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast").await,
            (NoticeTone::Neutral, "requested: off".to_owned())
        );
    }

    #[tokio::test]
    async fn a_picked_model_applies_its_effort_and_fast_mode_and_saves_them_together() {
        let codex = FakeServer::start([codex_text("picked")]);
        let catalog = codex_catalog(true, 1);
        let settings = json!({
            "provider": "codex",
            "models": {"codex": CODEX_MODEL},
            "session_titles": false
        });
        let mut harness = Harness::codex_saved(&codex, &catalog, &settings).await;
        let ModelCatalog::Listed { models, .. } = listed_catalog(&mut harness).await else {
            panic!("the Codex catalog lists its models");
        };
        assert_eq!(models[1].id, OTHER_CODEX_MODEL);
        assert_eq!(models[1].capabilities.reasoning_efforts, ["low"]);
        assert!(models[1].capabilities.supports_fast_mode);
        harness.send(select(OTHER_CODEX_MODEL, low(), Some(true)));
        let picked = harness
            .until(|event| matches!(event, UiEvent::ModelSelected { .. }))
            .await;
        assert_eq!(
            notice_body(picked),
            [format!("|Switched to {OTHER_CODEX_MODEL}")]
        );
        harness.submit("go");
        harness.until(finished(TurnOutcome::Completed)).await;
        let request = codex.requests()[0].json();
        assert_eq!(request["model"], OTHER_CODEX_MODEL);
        assert_eq!(request["reasoning"]["effort"], "low");
        assert_eq!(request["service_tier"], "priority");
        let saved = saved_settings(&harness);
        assert_eq!(saved["models"]["codex"], OTHER_CODEX_MODEL);
        assert_eq!(saved["effort"], "low");
        assert_eq!(saved["fast_mode"], true);
        assert_eq!(saved["fast_mode_model_bound"], true);
        let session = &saved_sessions(&harness.home)[0];
        assert_eq!(session["model"], OTHER_CODEX_MODEL);
        assert_eq!(session["effort"], "low");
        assert_eq!(session["fast_mode"], true);
    }

    fn codex_delegate(task: &str) -> Reply {
        let arguments = json!({"request": {"action": "run", "task": task}}).to_string();
        let events = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"subagent","arguments":""}}),
            json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":arguments}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}),
        ]
        .map(|event| event.to_string());
        Reply::sse(&events)
    }

    #[tokio::test]
    async fn a_child_started_after_a_pick_takes_the_picked_model_effort_and_fast_mode() {
        let codex = FakeServer::start([
            codex_delegate("summarise the notes"),
            codex_text("child done"),
            codex_text("parent done"),
        ]);
        let catalog = codex_catalog(true, 1);
        let settings = json!({
            "provider": "codex",
            "models": {"codex": CODEX_MODEL},
            "session_titles": false
        });
        let mut harness = Harness::codex_saved(&codex, &catalog, &settings).await;
        listed_catalog(&mut harness).await;
        harness.send(select(OTHER_CODEX_MODEL, low(), Some(true)));
        harness
            .until(|event| matches!(event, UiEvent::ModelSelected { .. }))
            .await;
        harness.submit("delegate the summary");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = codex.requests();
        assert_eq!(requests.len(), 3);
        for request in requests {
            let body = request.json();
            assert_eq!(body["model"], OTHER_CODEX_MODEL, "{body}");
            assert_eq!(body["reasoning"]["effort"], "low", "{body}");
            assert_eq!(body["service_tier"], "priority", "{body}");
        }
    }

    #[tokio::test]
    async fn a_choice_the_model_cannot_take_explains_the_usage_and_changes_nothing() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(false, 1);
        let mut harness = Harness::codex(&codex, &catalog).await;
        for choice in [
            select(CODEX_MODEL, ReasoningEffort::Named("max".to_owned()), None),
            select(CODEX_MODEL, low(), Some(true)),
        ] {
            harness.send(choice);
            let shown = harness
                .until(|event| matches!(event, UiEvent::Notice { .. }))
                .await;
            assert_eq!(
                notice_body(shown),
                ["|usage: /model <id> <effort> [normal|fast]"]
            );
        }
        harness.send(select(OTHER_CODEX_MODEL, ReasoningEffort::Auto, None));
        let picked = harness
            .until(|event| matches!(event, UiEvent::ModelSelected { .. }))
            .await;
        assert_eq!(
            notice_body(picked),
            [format!("|Switched to {OTHER_CODEX_MODEL}")]
        );
        let saved = saved_settings(&harness);
        assert_eq!(saved["effort"], "auto");
        assert_eq!(saved["fast_mode"], false);
    }

    #[tokio::test]
    async fn a_model_picked_during_a_turn_applies_to_the_next_one() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([held, Reply::sse(&chat_text_events(&["ok"]))]);
        let mut harness = Harness::start(&server).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        harness.send(select("vendor/model-b", ReasoningEffort::Auto, None));
        let notice = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(notice_body(notice), ["|Next turn will use vendor/model-b"]);
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
        harness.submit("next");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = server.requests();
        assert_eq!(requests[0].json()["model"], "model-a");
        assert_eq!(requests[1].json()["model"], "vendor/model-b");
    }

    async fn within<T>(work: impl Future<Output = T>) -> T {
        timeout(Duration::from_secs(10), work)
            .await
            .expect("the controller keeps serving the turn")
    }

    #[tokio::test]
    async fn model_changes_wait_for_a_cold_catalog_without_holding_up_the_turn() {
        let changes = [
            UiCommand::RunCommand {
                text: "/model luna".to_owned(),
            },
            select(OTHER_CODEX_MODEL, low(), None),
        ];
        for change in changes {
            let streamed = Gate::default();
            let listed = Gate::default();
            let codex = FakeServer::start([codex_partial().after(&streamed), codex_text("next")]);
            let catalog =
                FakeServer::start([catalog_version(), catalog_listing(false).after(&listed)]);
            let mut harness = Harness::codex(&codex, &catalog).await;
            harness.submit("slow");
            within(async {
                while codex.requests().is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            harness.send(change);
            streamed.open();
            within(harness.until(|event| matches!(event, UiEvent::AssistantText { .. }))).await;
            let turn_id = harness.running_turn();
            harness.send(UiCommand::Cancel { turn_id });
            within(harness.until(finished(TurnOutcome::Interrupted))).await;
            assert!(
                !harness
                    .seen
                    .iter()
                    .any(|event| matches!(event, UiEvent::ModelSelected { .. }))
            );
            listed.open();
            let picked =
                within(harness.until(|event| matches!(event, UiEvent::ModelSelected { .. }))).await;
            assert_eq!(
                notice_body(picked),
                [format!("|Switched to {OTHER_CODEX_MODEL}")]
            );
            harness.submit("next");
            within(harness.until(finished(TurnOutcome::Completed))).await;
            assert_eq!(codex.requests()[1].json()["model"], OTHER_CODEX_MODEL);
        }
    }

    #[tokio::test]
    async fn model_changes_waiting_for_the_catalog_apply_in_order_once_it_arrives_mid_turn() {
        let listed = Gate::default();
        let codex = FakeServer::start([codex_partial(), codex_text("next")]);
        let catalog = FakeServer::start([catalog_version(), catalog_listing(false).after(&listed)]);
        let mut harness = Harness::codex(&codex, &catalog).await;
        harness.submit("slow");
        within(harness.until(|event| matches!(event, UiEvent::AssistantText { .. }))).await;
        harness.command("/model luna");
        harness.send(select(CODEX_MODEL, low(), None));
        listed.open();
        let applied = within(harness.until(catalog_event)).await;
        assert_eq!(
            notice_body(applied),
            [
                format!("|Next turn will use {OTHER_CODEX_MODEL}"),
                format!("|Next turn will use {CODEX_MODEL}"),
            ]
        );
        assert!(
            !harness
                .seen
                .iter()
                .any(|event| matches!(event, UiEvent::TurnFinished { .. }))
        );
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
        harness.submit("next");
        harness.until(finished(TurnOutcome::Completed)).await;
        let request = codex.requests()[1].json();
        assert_eq!(request["model"], CODEX_MODEL);
        assert_eq!(request["reasoning"]["effort"], "low");
    }

    #[tokio::test]
    async fn catalog_changes_and_install_completion_keep_a_queued_prompt_in_order() {
        let listed = Gate::default();
        let codex = FakeServer::start([codex_partial(), codex_text("next")]);
        let catalog = FakeServer::start([catalog_version(), catalog_listing(false).after(&listed)]);
        let mut harness = Harness::codex(&codex, &catalog).await;
        write_skill(&harness.home, "install-pack", "new-skill");
        let source = fs::canonicalize(harness.home.path().join("workspace/install-pack")).unwrap();
        let (release, worker) = held_install_lock(&harness.home);
        harness.submit("active");
        within(harness.until(|event| matches!(event, UiEvent::AssistantText { .. }))).await;
        harness.command(&format!("/skills install {}", source.display()));
        harness.submit("queued");
        harness.command("/skills show new-skill");
        harness.command("/model luna");
        listed.open();
        let applied = within(harness.until(catalog_event)).await;
        assert_eq!(
            notice_body(applied),
            [format!("|Next turn will use {OTHER_CODEX_MODEL}")]
        );
        assert!(
            !harness
                .seen
                .iter()
                .any(|event| matches!(event, UiEvent::SkillsMenu { .. }))
        );
        harness.send(UiCommand::Cancel {
            turn_id: harness.running_turn(),
        });
        within(harness.until(finished(TurnOutcome::Interrupted))).await;
        release.send(()).unwrap();
        worker.join().unwrap();
        let shown =
            within(harness.until(|event| matches!(event, UiEvent::SkillsMenu { .. }))).await;
        assert_eq!(
            notice_body(shown),
            [
                format!("skills|Installing from {}...", source.display()),
                "skills|Installed: new-skill".to_owned(),
            ]
        );
        assert_eq!(codex.requests().len(), 1);
        within(harness.until(finished(TurnOutcome::Completed))).await;
        let requests = codex.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].json()["model"], OTHER_CODEX_MODEL);
        assert!(requests[1].json().to_string().contains("queued"));
    }

    #[tokio::test]
    async fn the_catalog_reaches_the_shell_while_a_turn_streams() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([held]);
        let mut harness = Harness::start(&server).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        let listed = listed_catalog(&mut harness).await;
        assert!(matches!(listed, ModelCatalog::Listed { .. }), "{listed:?}");
        assert!(
            !harness
                .seen
                .iter()
                .any(|event| matches!(event, UiEvent::TurnFinished { .. }))
        );
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
    }

    fn switching_settings(provider: &str, local: &FakeServer, other: &FakeServer) -> Value {
        let connection = |server: &FakeServer, models: &[&str]| {
            json!({
                "protocol": "openai-chat-completions",
                "base_url": server.base_url(),
                "auth": {"type": "none"},
                "models": models,
            })
        };
        json!({
            "provider": provider,
            "models": {"codex": CODEX_MODEL},
            "session_titles": false,
            "providers": {
                "local": connection(local, &["model-a", "vendor/model-b"]),
                "other": connection(other, &["other-model"]),
            }
        })
    }

    fn select_provider(provider: &str) -> UiCommand {
        UiCommand::SelectProvider {
            provider: provider.to_owned(),
        }
    }

    fn provider_notice(event: &UiEvent) -> bool {
        matches!(event, UiEvent::Notice { notice } if notice.topic == "provider"
            && !notice.body.starts_with("Preparing"))
    }

    async fn switched(harness: &mut Harness, provider: &str) -> Vec<String> {
        harness.send(select_provider(provider));
        let shown = harness.until(provider_notice).await;
        notice_body(shown)
    }

    #[tokio::test]
    async fn provider_commands_open_the_column_and_wait_for_running_work() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let local = FakeServer::start([held]);
        let other = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let settings = switching_settings("local", &local, &other);
        let setup = agent_setup_with(&home, &settings, SubscriptionEndpoints::default()).await;
        let mut harness = Harness::with_setup(home, setup);
        for (command, prefix) in [
            ("/provider", "/provider "),
            ("/setup", "/provider "),
            ("/login", "/login "),
        ] {
            harness.command(command);
            let opened = harness
                .until(|event| matches!(event, UiEvent::ProviderPicker { .. }))
                .await;
            assert_eq!(
                opened.last(),
                Some(&UiEvent::ProviderPicker {
                    prefix: prefix.to_owned(),
                    providers: vec!["codex".to_owned(), "local".to_owned(), "other".to_owned()],
                })
            );
        }
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        let busy =
            "provider|Provider switching is unavailable until active and queued work finishes.";
        harness.command("/provider");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(notice_body(shown), [busy]);
        assert_eq!(switched(&mut harness, "other").await, [busy]);
        for (provider, notice) in [
            (
                "codex",
                "auth|Codex sign-in is unavailable until active and queued work finishes.",
            ),
            ("other", busy),
        ] {
            harness.send(UiCommand::SignIn {
                provider: provider.to_owned(),
            });
            let shown = harness
                .until(|event| matches!(event, UiEvent::Notice { .. }))
                .await;
            assert_eq!(notice_body(shown), [notice]);
        }
        assert!(other.requests().is_empty());
    }

    #[tokio::test]
    async fn a_session_switched_to_codex_is_named_with_the_codex_title_model() {
        let codex = FakeServer::start([
            codex_text("Fix the renderer"),
            codex_text("Fix the renderer"),
        ]);
        let catalog = codex_catalog(false, 4);
        let local = FakeServer::start([]);
        let other = FakeServer::start([]);
        let mut settings = switching_settings("local", &local, &other);
        settings["session_titles"] = json!(true);
        let home = codex_home();
        let setup = agent_setup_with(&home, &settings, codex_endpoints(&codex, &catalog)).await;
        let mut harness = Harness::saved(home, setup);
        harness.send(select_provider("codex"));
        harness.until(provider_notice).await;
        chat(&mut harness, &["please fix the renderer"]).await;
        within(until_titled(&mut harness, "Fix the renderer")).await;
        assert_eq!(title_requests(&codex).len(), 1);
    }

    #[tokio::test]
    async fn a_switch_moves_the_conversation_and_keeps_fast_mode_and_effort() {
        let codex = FakeServer::start([codex_text("one"), codex_text("three")]);
        let catalog = codex_catalog(true, 4);
        let local = FakeServer::start([Reply::sse(&chat_text_events(&["two"]))]);
        let other = FakeServer::start([]);
        let settings = switching_settings("codex", &local, &other);
        let endpoints = codex_endpoints(&codex, &catalog);
        let home = codex_home();
        let setup = agent_setup_with(&home, &settings, endpoints).await;
        let mut harness = Harness::saved(home, setup);
        harness.send(select(OTHER_CODEX_MODEL, low(), Some(true)));
        harness
            .until(|event| matches!(event, UiEvent::ModelSelected { .. }))
            .await;
        harness.submit("one");
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.send(select_provider("local"));
        let shown = harness.until(provider_notice).await;
        assert_eq!(
            notice_body(shown),
            [
                "provider|Preparing local.",
                "provider|Switched to local with model-a."
            ]
        );
        assert!(shown.contains(&UiEvent::ProviderSelected {
            provider: "local".to_owned()
        }));
        assert!(shown.contains(&UiEvent::ModelSelected {
            model: "model-a".to_owned()
        }));
        let saved = saved_settings(&harness);
        assert_eq!(saved["provider"], "local");
        assert_eq!(saved["models"]["local"], "model-a");
        assert_eq!(saved["models"]["codex"], OTHER_CODEX_MODEL);
        assert_eq!(saved["effort"], "low");
        assert_eq!(saved["fast_mode"], true);
        assert_eq!(saved.get("fast_mode_model_bound"), None);
        let session = &saved_sessions(&harness.home)[0];
        assert_eq!(session["provider"]["name"], "local");
        assert_eq!(session["model"], "model-a");
        assert_eq!(session["effort"], "low");
        assert_eq!(session["fast_mode"], true);
        harness.submit("two");
        harness.until(finished(TurnOutcome::Completed)).await;
        let request = local.requests()[0].json();
        assert_eq!(request["model"], "model-a");
        assert_eq!(user_messages(&request), 2);
        assert_eq!(
            switched(&mut harness, "codex").await,
            [
                "provider|Preparing Codex subscription.".to_owned(),
                format!("provider|Switched to Codex subscription with {OTHER_CODEX_MODEL}.")
            ]
        );
        harness.submit("three");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = codex.requests();
        let request = requests[1].json();
        assert_eq!(request["model"], OTHER_CODEX_MODEL);
        assert_eq!(request["reasoning"]["effort"], "low");
        assert_eq!(request["service_tier"], "priority");
        assert_eq!(saved_sessions(&harness.home)[0]["provider"], "codex");
    }

    #[tokio::test]
    async fn a_child_started_after_a_switch_runs_on_the_new_provider() {
        let local = FakeServer::start([]);
        let other = FakeServer::start([
            delegate("read the notes"),
            Reply::sse(&chat_text_events(&["child done"])),
            Reply::sse(&chat_text_events(&["parent done"])),
        ]);
        let home = tempfile::tempdir().unwrap();
        let settings = switching_settings("local", &local, &other);
        let setup = agent_setup_with(&home, &settings, SubscriptionEndpoints::default()).await;
        let mut harness = Harness::saved(home, setup);
        assert_eq!(
            switched(&mut harness, "other").await,
            [
                "provider|Preparing other.",
                "provider|Switched to other with other-model."
            ]
        );
        harness.submit("delegate the reading");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert!(local.requests().is_empty());
        let requests = other.requests();
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| request.json()["model"] == "other-model")
        );
        assert!(requests[1].body_text().contains("read the notes"));
    }

    #[tokio::test]
    async fn a_settings_failure_is_reported_before_preparing_and_changes_nothing() {
        let local = FakeServer::start([]);
        let other = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let settings = switching_settings("local", &local, &other);
        let setup = agent_setup_with(&home, &settings, SubscriptionEndpoints::default()).await;
        let mut harness = Harness::with_setup(home, setup);
        assert_eq!(
            switched(&mut harness, "local").await,
            ["provider|Already using local."]
        );
        let path = harness.home.path().join("config/settings.json");
        fs::write(&path, "{not json").unwrap();
        harness.send(select_provider("other"));
        let shown = harness.until(provider_notice).await;
        assert_eq!(
            notice_body(shown),
            [
                "provider|Could not load the saved provider selection. The current provider is unchanged."
            ]
        );
        fs::write(&path, settings.to_string()).unwrap();
        assert_eq!(
            switched(&mut harness, "missing").await,
            [
                "provider|The target provider catalog is unavailable. The current provider is unchanged."
            ]
        );
        assert!(
            harness
                .seen
                .iter()
                .all(|event| !matches!(event, UiEvent::ProviderSelected { .. }))
        );
    }

    #[tokio::test]
    async fn a_catalog_requested_before_a_switch_is_never_delivered_after_it() {
        let gate = Gate::default();
        let codex = FakeServer::start([]);
        let catalog = FakeServer::start([catalog_version(), catalog_listing(true).after(&gate)]);
        let local = FakeServer::start([]);
        let other = FakeServer::start([]);
        let settings = switching_settings("codex", &local, &other);
        let endpoints = codex_endpoints(&codex, &catalog);
        let home = codex_home();
        let setup = agent_setup_with(&home, &settings, endpoints).await;
        let mut harness = Harness::with_setup(home, setup);
        harness.send(UiCommand::ListModels);
        while catalog.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            switched(&mut harness, "local").await,
            [
                "provider|Preparing local.",
                "provider|Switched to local with model-a."
            ]
        );
        gate.open();
        harness.send(UiCommand::ListModels);
        let delivered = harness.until(catalog_event).await;
        let catalogs: Vec<&str> = delivered
            .iter()
            .filter_map(|event| match event {
                UiEvent::ModelCatalog { provider, .. } => Some(provider.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(catalogs, ["local"]);
    }

    #[tokio::test]
    async fn a_model_query_resolves_against_the_codex_catalog() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(false, 1);
        let mut harness = Harness::codex(&codex, &catalog).await;
        harness.command("/model luna");
        let picked = harness
            .until(|event| matches!(event, UiEvent::ModelSelected { .. }))
            .await;
        assert_eq!(
            notice_body(picked),
            [format!("|Switched to {OTHER_CODEX_MODEL}")]
        );
    }

    #[tokio::test]
    async fn internally_loaded_capabilities_update_controls_during_a_held_turn() {
        let codex = FakeServer::start([codex_partial()]);
        let catalog = codex_catalog(false, 1);
        let home = codex_home();
        let settings = codex_settings();
        let mut setup = agent_setup_with(&home, &settings, codex_endpoints(&codex, &catalog)).await;
        setup.restore_reasoning(Some("high".to_owned()), false);
        assert_eq!(
            setup.model_controls().effort,
            ReasoningEffort::Named("high".to_owned())
        );
        assert!(setup.models_source().cached().is_none());
        let mut harness = Harness::with_setup(home, setup);
        harness.submit("hello");
        within(harness.until(|event| matches!(event, UiEvent::AssistantText { .. }))).await;
        assert_eq!(codex.requests()[0].json().get("reasoning"), None);
        let updated = |event: &UiEvent| matches!(event, UiEvent::ModelControlsChanged { controls } if controls.effort == ReasoningEffort::Auto && controls.effort_supported);
        if !harness.seen.iter().any(updated) {
            let result = timeout(Duration::from_secs(10), harness.until(updated)).await;
            assert!(
                result.is_ok(),
                "controls arrive while held: {:?}",
                harness.seen
            );
        }
        assert!(
            !harness
                .seen
                .iter()
                .any(|event| matches!(event, UiEvent::TurnFinished { .. }))
        );
        assert_eq!(
            harness.seen.iter().filter(|event| updated(event)).count(),
            1
        );
        let turn_id = harness
            .seen
            .iter()
            .find_map(|event| match event {
                UiEvent::TurnStarted { turn_id, .. } => Some(*turn_id),
                _ => None,
            })
            .unwrap();
        harness.send(UiCommand::Cancel { turn_id });
        timeout(
            Duration::from_secs(10),
            harness.until(finished(TurnOutcome::Interrupted)),
        )
        .await
        .expect("cancel settles held response");
    }

    #[tokio::test]
    async fn a_populated_catalog_is_ready_before_the_waiter_starts() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(false, 1);
        let home = codex_home();
        let setup =
            agent_setup_with(&home, &codex_settings(), codex_endpoints(&codex, &catalog)).await;
        let source = setup.models_source();
        assert!(source.cached().is_none());
        assert!(matches!(
            source.catalog().await,
            ModelCatalog::Listed { .. }
        ));
        within(source.ready()).await;
        assert_eq!(catalog.requests().len(), 2);
        assert!(codex.requests().is_empty());
    }

    fn controls_changed(event: &UiEvent) -> bool {
        matches!(event, UiEvent::ModelControlsChanged { .. })
    }

    fn last_controls(events: &[UiEvent]) -> ModelControls {
        events
            .iter()
            .rev()
            .find_map(|event| match event {
                UiEvent::ModelControlsChanged { controls } => Some(controls.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn the_status_line_follows_the_session_s_effort_and_fast_mode() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(true, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        listed_catalog(&mut harness).await;
        let listed = within(harness.until(controls_changed)).await;
        assert_eq!(
            last_controls(listed),
            ModelControls {
                effort: ReasoningEffort::Auto,
                effort_supported: true,
                fast: false,
            }
        );
        harness.send(select(OTHER_CODEX_MODEL, low(), Some(true)));
        let picked = within(harness.until(controls_changed)).await;
        assert_eq!(
            last_controls(picked),
            ModelControls {
                effort: low(),
                effort_supported: true,
                fast: true,
            }
        );
        harness.command("/fast");
        let toggled = within(harness.until(controls_changed)).await;
        assert_eq!(
            last_controls(toggled),
            ModelControls {
                effort: low(),
                effort_supported: true,
                fast: false,
            }
        );
    }

    #[tokio::test]
    async fn fast_mode_and_the_model_choice_are_saved_to_user_settings() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(true, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        assert_eq!(fast_notice(&mut harness).await, "fast|on");
        let saved = saved_settings(&harness);
        assert_eq!(saved["provider"], "codex");
        assert_eq!(saved["models"]["codex"], CODEX_MODEL);
        assert_eq!(saved["fast_mode"], true);
        assert_eq!(saved["fast_mode_model_bound"], true);
        harness.command(&format!("/model {OTHER_CODEX_MODEL}"));
        harness.command("/version");
        assert_eq!(
            notices_until(&mut harness, "version").await[0],
            format!("|Switched to {OTHER_CODEX_MODEL}")
        );
        let saved = saved_settings(&harness);
        assert_eq!(saved["models"]["codex"], OTHER_CODEX_MODEL);
        assert_eq!(saved["fast_mode"], false);
        assert_eq!(fast_notice(&mut harness).await, "fast|on");
        assert_eq!(saved_settings(&harness)["fast_mode"], true);
        assert_eq!(fast_notice(&mut harness).await, "fast|off");
        assert_eq!(saved_settings(&harness)["fast_mode"], false);
    }

    #[tokio::test]
    async fn a_choice_user_settings_cannot_hold_still_applies_and_says_so() {
        let codex = FakeServer::start([codex_text("fast")]);
        let catalog = codex_catalog(true, 8);
        let mut harness = Harness::codex(&codex, &catalog).await;
        let settings = harness.home.path().join("config/settings.json");
        fs::write(&settings, r#"{"workspaces":"legacy"}"#).unwrap();
        harness.command("/fast");
        assert_eq!(
            notices_until(&mut harness, "fast").await,
            ["fast|active for this process but not saved to user settings (InvalidSettingsFormat)"]
        );
        assert_eq!(notices_until(&mut harness, "fast").await, ["fast|on"]);
        harness.command(&format!("/model {CODEX_MODEL}"));
        assert_eq!(
            notices_until(&mut harness, "model").await,
            [
                format!("|Switched to {CODEX_MODEL}"),
                "model|active for this process but not saved to user settings (InvalidSettingsFormat)".to_owned()
            ]
        );
        harness.submit("hurry");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(codex.requests()[0].json()["service_tier"], "priority");
        assert_eq!(
            fs::read_to_string(&settings).unwrap(),
            r#"{"workspaces":"legacy"}"#
        );
    }

    #[tokio::test]
    async fn version_reports_the_running_build() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        harness.command("/version");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(
            notice_body(shown),
            [format!("version|{}", ofx_upgrade::VERSION)]
        );
        let Some(UiEvent::Notice { notice }) = shown.last() else {
            unreachable!()
        };
        assert_eq!(notice.tone, NoticeTone::Neutral);
    }

    #[tokio::test]
    async fn usage_and_cost_report_that_durable_profile_usage_is_unavailable() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        for command in ["/usage", "/cost"] {
            harness.command(command);
            let shown = harness
                .until(|event| matches!(event, UiEvent::Notice { .. }))
                .await;
            assert_eq!(
                notice_body(shown),
                [
                    "usage|Durable profile usage is unavailable in this host; active session usage remains in memory."
                ]
            );
        }
    }

    #[tokio::test]
    async fn alias_reports_that_aliases_are_not_yet_configurable_for_every_form() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        for command in ["/alias", "/alias gs", "/alias gs git status"] {
            harness.command(command);
            let shown = harness
                .until(|event| matches!(event, UiEvent::Notice { .. }))
                .await;
            assert_eq!(
                notice_body(shown),
                ["aliases|Aliases are not yet configurable."],
                "{command}"
            );
        }
    }

    #[tokio::test]
    async fn the_allowlist_takes_the_subagent_tool_as_upstream_does() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        harness.command("/allowlist add tool subagent");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(
            notice_body(shown),
            [r#"allowlist|added tool subagent: "*" (scope=local)"#]
        );
    }

    #[tokio::test]
    async fn statusline_starts_from_user_settings_and_toggles_the_saved_items() {
        let server = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let settings = json!({
            "provider": "local",
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": server.base_url(),
                    "auth": {"type": "none"},
                    "models": ["model-a"]
                }
            },
            "statusLine": {"context": true}
        });
        let setup = agent_setup_with(&home, &settings, SubscriptionEndpoints::default()).await;
        assert!(setup.statusline().enabled(StatuslineItem::Context));
        let mut harness = Harness::with_setup(home, setup);
        harness.command("/statusline");
        harness
            .until(|event| matches!(event, UiEvent::StatuslineMenuOpened))
            .await;
        let toggle = || UiCommand::ToggleStatusline {
            item: StatuslineItem::Session,
        };
        harness.send(toggle());
        harness.send(toggle());
        harness.send(toggle());
        let changed = |event: &UiEvent| matches!(event, UiEvent::StatuslineChanged { .. });
        let mut shown = Vec::new();
        for _ in 0..3 {
            shown.extend(harness.until(changed).await.iter().cloned());
        }
        let session = |enabled| UiEvent::StatuslineChanged {
            item: StatuslineItem::Session,
            enabled,
        };
        assert_eq!(shown, [session(true), session(false), session(true)]);
        assert_eq!(
            saved_settings(&harness)["statusLine"],
            json!({"context": true, "session": true})
        );
        harness.command("/statusline context");
        let shown = harness
            .until(|event| {
                matches!(event, UiEvent::Notice { notice } if notice.body == "context: off")
            })
            .await;
        assert_eq!(
            shown[0],
            UiEvent::StatuslineChanged {
                item: StatuslineItem::Context,
                enabled: false,
            }
        );
        assert_eq!(
            notice_body(shown),
            [
                "statusline|saved to user settings (scope=user)",
                "statusline|context: off",
            ]
        );
        assert_eq!(
            saved_settings(&harness)["statusLine"],
            json!({"context": false, "session": true})
        );
        harness.command("/statusline workspace");
        notices_until(&mut harness, "statusline").await;
        harness.command("/statusline sandbox");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { notice } if notice.topic.is_empty()))
            .await;
        assert_eq!(
            notice_body(shown).last().unwrap(),
            "|usage: /statusline [context|session|workspace]"
        );
        assert_eq!(
            saved_settings(&harness)["statusLine"],
            json!({"context": false, "session": true, "workspace": true})
        );
    }

    #[tokio::test]
    async fn shell_reload_answers_from_the_slash_registry() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        harness.command("/shell reload");
        assert_eq!(
            notices_until(&mut harness, "shell").await,
            [
                "shell|The next command reloads your shell startup files. Remembered command approvals were reset."
            ]
        );
        harness.command("/shell");
        assert_eq!(
            notices_until(&mut harness, "").await,
            ["|usage: /shell reload"]
        );
    }

    async fn undo_notice(harness: &mut Harness) -> String {
        harness.command("/undo");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        notice_body(shown).pop().unwrap()
    }

    fn write_call(id: &str, path: &str, content: &str) -> Reply {
        let arguments = json!({"path": path, "content": content}).to_string();
        Reply::sse(&chat_tool_call_events(id, "write_file", &arguments))
    }

    #[tokio::test]
    async fn undo_reverses_the_session_file_changes_newest_first_until_a_clear() {
        let server = FakeServer::start([
            write_call("call-1", "notes.md", "changed\n"),
            write_call("call-2", "fresh.md", "fresh\n"),
            Reply::sse(&chat_text_events(&["Wrote both."])),
            write_call("call-3", "notes.md", "again\n"),
            Reply::sse(&chat_text_events(&["Wrote it again."])),
        ]);
        let mut harness = Harness::start(&server).await;
        let workspace = fs::canonicalize(harness.home.path().join("workspace")).unwrap();
        let notes = workspace.join("notes.md");
        fs::write(&notes, "original\n").unwrap();
        assert_eq!(undo_notice(&mut harness).await, "undo|Nothing to undo.");
        harness.submit("write the notes");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(
            undo_notice(&mut harness).await,
            format!(
                "undo|Deleted {} (was newly created)",
                workspace.join("fresh.md").display()
            )
        );
        assert!(!workspace.join("fresh.md").exists());
        assert_eq!(
            undo_notice(&mut harness).await,
            format!("undo|Restored {}", notes.display())
        );
        assert_eq!(fs::read_to_string(&notes).unwrap(), "original\n");
        assert_eq!(undo_notice(&mut harness).await, "undo|Nothing to undo.");
        harness.submit("write them again");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(fs::read_to_string(&notes).unwrap(), "again\n");
        harness.command("/clear");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        assert_eq!(undo_notice(&mut harness).await, "undo|Nothing to undo.");
        assert_eq!(fs::read_to_string(&notes).unwrap(), "again\n");
    }

    #[tokio::test]
    async fn cancels_naming_a_finished_turn_leave_the_running_turn_alone() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"])), held]);
        let mut harness = Harness::start(&server).await;
        harness.submit("first");
        harness.until(finished(TurnOutcome::Completed)).await;
        let stale = harness.running_turn();
        harness.submit("second");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        let running = harness.running_turn();
        assert_ne!(stale, running);
        harness.send(UiCommand::Cancel { turn_id: stale });
        let early = timeout(
            Duration::from_millis(300),
            harness.until(finished(TurnOutcome::Interrupted)),
        )
        .await;
        assert!(
            early.is_err(),
            "a stale cancel interrupted the running turn"
        );
        harness.send(UiCommand::Cancel { turn_id: running });
        let interrupted = harness.until(finished(TurnOutcome::Interrupted)).await;
        assert!(matches!(
            interrupted.last(),
            Some(UiEvent::TurnFinished { turn_id, .. }) if *turn_id == running
        ));
    }

    #[tokio::test]
    async fn help_exit_and_unknown_commands_produce_ui_events() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        harness.command("/help");
        harness.command("/bogus");
        harness.command("/exit");
        let events = harness
            .until(|event| matches!(event, UiEvent::ExitRequested))
            .await
            .to_vec();
        assert_eq!(events[0], UiEvent::HelpRequested);
        assert_eq!(
            notice_body(&events),
            ["command|Unknown command. Try /help."]
        );
    }

    #[tokio::test]
    async fn provider_failures_are_reported_for_their_turn_before_it_finishes() {
        let server = FakeServer::start([Reply::status(401, r#"{"error":{"message":"bad key"}}"#)]);
        let mut harness = Harness::start(&server).await;
        harness.submit("hi");
        let events = harness.until(finished(TurnOutcome::Failed)).await.to_vec();
        let turn_id = harness.running_turn();
        assert_eq!(
            events[events.len() - 2..],
            [
                UiEvent::ApiStatus {
                    turn_id,
                    text: "⚠ configured provider authentication failed · HTTP 401 · Check the configured provider auth environment variable.".to_owned()
                },
                UiEvent::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Failed
                }
            ]
        );
    }

    #[tokio::test]
    async fn approvals_from_the_shell_let_gated_reads_run_and_denials_reach_the_model() {
        for (decision, status) in [
            (ApprovalDecision::Once, ToolResultStatus::Success),
            (ApprovalDecision::Deny, ToolResultStatus::Failure),
        ] {
            let server = FakeServer::start([
                Reply::sse(&chat_tool_call_events(
                    "call-1",
                    "read_file",
                    r#"{"path":"../outside.txt"}"#,
                )),
                Reply::sse(&chat_text_events(&["done"])),
            ]);
            let mut harness = Harness::start(&server).await;
            fs::write(harness.home.path().join("outside.txt"), "secret notes\n").unwrap();
            harness.submit("read it");
            let requested = harness
                .until(|event| matches!(event, UiEvent::ApprovalRequested { .. }))
                .await;
            let Some(UiEvent::ApprovalRequested { request, .. }) = requested.last().cloned() else {
                unreachable!()
            };
            assert_eq!(request.tool_name, "read_file");
            assert_eq!(request.description.title, "Reading ../outside.txt");
            harness.send(UiCommand::Approval {
                request_id: request.id,
                answer: decision.into(),
            });
            let events = harness.until(finished(TurnOutcome::Completed)).await;
            assert!(events.iter().any(|event| matches!(
                event,
                UiEvent::ToolFinished { status: seen, .. } if *seen == status
            )));
            let body = server.requests()[1].json();
            let tool_result = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "tool")
                .and_then(|message| message["content"].as_str())
                .unwrap();
            assert_eq!(
                tool_result.contains("secret notes"),
                decision == ApprovalDecision::Once,
                "{tool_result}"
            );
            assert_eq!(
                tool_result.contains("tool_permission_denied"),
                decision == ApprovalDecision::Deny,
                "{tool_result}"
            );
        }
    }

    fn outside_read() -> Reply {
        Reply::sse(&chat_tool_call_events(
            "call-1",
            "read_file",
            r#"{"path":"../outside.txt"}"#,
        ))
    }

    fn mode_changed(mode: PermissionMode) -> impl Fn(&UiEvent) -> bool {
        move |event| matches!(event, UiEvent::PermissionModeChanged { mode: seen, .. } if *seen == mode)
    }

    fn approval_requested(event: &UiEvent) -> bool {
        matches!(event, UiEvent::ApprovalRequested { .. })
    }

    fn saved_permission_mode(harness: &Harness) -> Value {
        let settings =
            fs::read_to_string(harness.home.path().join("config/settings.json")).unwrap();
        serde_json::from_str::<Value>(&settings).unwrap()["permission_mode"].clone()
    }

    #[tokio::test]
    async fn toggling_the_mode_decides_the_next_tool_call_and_saves_the_mode() {
        let server = FakeServer::start([outside_read(), Reply::sse(&chat_text_events(&["done"]))]);
        let mut harness = Harness::start(&server).await;
        fs::write(harness.home.path().join("outside.txt"), "secret notes\n").unwrap();
        harness.send(UiCommand::TogglePermissionMode);
        let changed = harness.until(mode_changed(PermissionMode::Yolo)).await;
        assert_eq!(
            changed.last(),
            Some(&UiEvent::PermissionModeChanged {
                mode: PermissionMode::Yolo,
                full_access_warning: true
            })
        );
        assert_eq!(saved_permission_mode(&harness), "yolo");
        harness.submit("read it");
        let events = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(!events.iter().any(approval_requested), "{events:?}");
        let body = server.requests()[1].json();
        assert!(body.to_string().contains("secret notes"), "{body}");
        harness.send(UiCommand::TogglePermissionMode);
        harness.until(mode_changed(PermissionMode::Ask)).await;
        assert_eq!(saved_permission_mode(&harness), "ask");
    }

    #[tokio::test]
    async fn a_mode_switched_while_an_approval_is_held_applies_from_the_next_call() {
        let server = FakeServer::start([
            outside_read(),
            outside_read(),
            Reply::sse(&chat_text_events(&["done"])),
        ]);
        let mut harness = Harness::start(&server).await;
        fs::write(harness.home.path().join("outside.txt"), "secret notes\n").unwrap();
        harness.submit("read it twice");
        let Some(UiEvent::ApprovalRequested { request, .. }) =
            harness.until(approval_requested).await.last().cloned()
        else {
            unreachable!()
        };
        harness.command("/permissions full-access");
        let switched = harness.until(mode_changed(PermissionMode::Yolo)).await;
        assert!(
            !switched
                .iter()
                .any(|event| matches!(event, UiEvent::ToolFinished { .. }))
        );
        harness.send(UiCommand::Approval {
            request_id: request.id,
            answer: ApprovalDecision::Once.into(),
        });
        let rest = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(!rest.iter().any(approval_requested), "{rest:?}");
        let finished_reads = rest
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    UiEvent::ToolFinished {
                        status: ToolResultStatus::Success,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(finished_reads, 2);
        assert_eq!(notice_body(rest), ["permissions|mode set to full access"]);
    }

    #[tokio::test]
    async fn a_reset_sent_right_after_an_always_answer_forgets_the_grant_it_recorded() {
        let server = FakeServer::start([
            outside_read(),
            outside_read(),
            Reply::sse(&chat_text_events(&["done"])),
        ]);
        let mut harness = Harness::start(&server).await;
        fs::write(harness.home.path().join("outside.txt"), "secret notes\n").unwrap();
        harness.submit("read it twice");
        let Some(UiEvent::ApprovalRequested { request, .. }) =
            harness.until(approval_requested).await.last().cloned()
        else {
            unreachable!()
        };
        harness.send(UiCommand::Approval {
            request_id: request.id,
            answer: ApprovalDecision::Always.into(),
        });
        harness.command("/permissions reset");
        let asked = harness
            .until(|event| {
                approval_requested(event) || matches!(event, UiEvent::TurnFinished { .. })
            })
            .await;
        assert!(asked.last().is_some_and(approval_requested), "{asked:?}");
        assert_eq!(saved_permission_mode(&harness), "ask");
    }

    #[tokio::test]
    async fn clear_forgets_the_approvals_remembered_for_the_session() {
        let read = || {
            Reply::sse(&chat_tool_call_events(
                "call-1",
                "read_file",
                r#"{"path":"../outside.txt"}"#,
            ))
        };
        let server = FakeServer::start([
            read(),
            Reply::sse(&chat_text_events(&["done"])),
            read(),
            Reply::sse(&chat_text_events(&["done"])),
            read(),
            Reply::sse(&chat_text_events(&["done"])),
        ]);
        let mut harness = Harness::start(&server).await;
        fs::write(harness.home.path().join("outside.txt"), "secret notes\n").unwrap();
        let requested = |event: &UiEvent| matches!(event, UiEvent::ApprovalRequested { .. });
        harness.submit("read it");
        let Some(UiEvent::ApprovalRequested { request, .. }) =
            harness.until(requested).await.last().cloned()
        else {
            unreachable!()
        };
        assert!(request.scope.always.is_some());
        harness.send(UiCommand::Approval {
            request_id: request.id,
            answer: ApprovalDecision::Always.into(),
        });
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.submit("read it again");
        let remembered = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(!remembered.iter().any(requested));
        harness.command("/clear");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        harness.submit("read it after clearing");
        let asked = harness
            .until(|event| requested(event) || matches!(event, UiEvent::TurnFinished { .. }))
            .await;
        assert!(asked.last().is_some_and(requested), "{asked:?}");
    }

    #[test]
    fn context_notice_bodies_drop_legacy_markers_from_every_line() {
        let notice =
            context_notice("[context] first\n[context] second\nalready semantic\n[context]\n");
        assert_eq!(notice.body, "first\nsecond\nalready semantic\n\n");
        assert_eq!(notice.topic, "context");
        assert_eq!(notice.tone, NoticeTone::Warning);
        assert_eq!(context_notice("[context]x").body, "[context]x");
    }

    #[test]
    fn context_notices_show_once_per_conversation_and_again_after_a_clear() {
        let mut notices = ContextNotices {
            startup: vec![
                "[context] startup".to_owned(),
                "[context] startup".to_owned(),
            ],
            claimed: HashSet::new(),
        };
        let bodies = |shown: Vec<Notice>| -> Vec<String> {
            shown.into_iter().map(|notice| notice.body).collect()
        };
        let claimed =
            |notices: &mut ContextNotices, text| notices.claim(text).map(|notice| notice.body);
        assert_eq!(bodies(notices.restart()), ["startup"]);
        assert_eq!(claimed(&mut notices, "[context] startup"), None);
        assert_eq!(
            claimed(&mut notices, "[context] scoped").as_deref(),
            Some("scoped")
        );
        assert_eq!(claimed(&mut notices, "[context] scoped"), None);
        assert_eq!(bodies(notices.restart()), ["startup"]);
        assert_eq!(
            claimed(&mut notices, "[context] scoped").as_deref(),
            Some("scoped")
        );
    }

    fn write_skill(home: &tempfile::TempDir, directory: &str, name: &str) {
        let path = home
            .path()
            .join("workspace")
            .join(directory)
            .join("SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!("---\nname: {name}\ndescription: {name} workflow\n---\n{name} steps\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn capability_search_is_offered_for_zero_one_and_many_skills() {
        for count in [0, 1, 40] {
            let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
            let mut harness = Harness::start(&server).await;
            for index in 0..count {
                let name = format!("review-{index}");
                write_skill(&harness.home, &format!("skills/{name}"), &name);
            }
            harness.submit("find a capability");
            harness.until(finished(TurnOutcome::Completed)).await;
            let body = server.requests()[0].json();
            let offered: Vec<_> = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["function"]["name"] == "capability_search")
                .collect();
            assert_eq!(offered.len(), 1, "skill count: {count}");
            let names: Vec<_> = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap())
                .collect();
            let search_index = names
                .iter()
                .position(|name| *name == "capability_search")
                .unwrap();
            assert_eq!(names[search_index + 1], "skill");

            assert_eq!(
                offered[0]["function"]["parameters"],
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "minLength": 1, "maxLength": 4096,
                            "description": "Natural-language capability needed for the current task."},
                        "server": {"type": "string", "minLength": 1,
                            "description": "Optional exact configured MCP server alias."}
                    },
                    "additionalProperties": false,
                    "required": ["query"]
                })
            );
        }
    }

    #[tokio::test]
    async fn capability_search_returns_a_location_consumed_by_the_existing_skill_tool() {
        let home = tempfile::tempdir().unwrap();
        write_skill(&home, "skills/review", "review");
        let location = fs::canonicalize(home.path().join("workspace/skills/review"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let arguments = json!({"location": location}).to_string();
        let server = FakeServer::start([
            Reply::sse(&chat_tool_call_events(
                "search-1",
                "capability_search",
                r#"{"query":"review"}"#,
            )),
            Reply::sse(&chat_tool_call_events("skill-1", "skill", &arguments)),
            Reply::sse(&chat_text_events(&["done"])),
        ]);
        let setup = agent_setup(&home, &server).await;
        let mut harness = Harness::with_setup(home, setup);
        harness.submit("find and load review");
        timeout(
            Duration::from_secs(10),
            harness.until(|event| matches!(event, UiEvent::TurnFinished { .. })),
        )
        .await
        .unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        let search_body = requests[1].json();
        let search_output = search_body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_call_id"] == "search-1")
            .unwrap();
        let result: Value =
            serde_json::from_str(search_output["content"].as_str().unwrap()).unwrap();
        assert_eq!(result["skills"][0]["location"], location);
        assert_eq!(result["counts"], json!({"skills": 1, "mcp_tools": 0}));
        assert!(result.get("mcp_state").is_none());
        assert!(result.get("state").is_none());
        let loaded_body = requests[2].json();
        let loaded = loaded_body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_call_id"] == "skill-1")
            .unwrap();
        assert!(loaded["content"].as_str().unwrap().contains("review steps"));
        let requested: Value = serde_json::from_str(&arguments).unwrap();
        assert_eq!(requested["location"], result["skills"][0]["location"]);
    }

    #[tokio::test]
    async fn capability_search_server_scope_does_not_fall_back_to_skills() {
        let server = FakeServer::start([
            Reply::sse(&chat_tool_call_events(
                "search-1",
                "capability_search",
                r#"{"query":"review","server":"absent"}"#,
            )),
            Reply::sse(&chat_text_events(&["done"])),
        ]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, "skills/review", "review");
        harness.submit("search the absent server");
        timeout(
            Duration::from_secs(10),
            harness.until(|event| matches!(event, UiEvent::TurnFinished { .. })),
        )
        .await
        .unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        let body = requests[1].json();
        let output = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_call_id"] == "search-1")
            .unwrap();
        let result: Value = serde_json::from_str(output["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            result,
            json!({
                "skills": [], "mcp_tools": [],
                "counts": {"skills": 0, "mcp_tools": 0},
                "total_matches": {"skills": 0, "mcp_tools": 0},
                "state": "no_match"
            })
        );
    }

    #[tokio::test]
    async fn capability_search_non_matching_query_reports_no_match() {
        let server = FakeServer::start([
            Reply::sse(&chat_tool_call_events(
                "search-1",
                "capability_search",
                r#"{"query":"absent"}"#,
            )),
            Reply::sse(&chat_text_events(&["done"])),
        ]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, "skills/review", "review");
        harness.submit("search the absent server");
        timeout(
            Duration::from_secs(10),
            harness.until(|event| matches!(event, UiEvent::TurnFinished { .. })),
        )
        .await
        .unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        let body = requests[1].json();
        let output = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_call_id"] == "search-1")
            .unwrap();
        let result: Value = serde_json::from_str(output["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            result,
            json!({
                "skills": [], "mcp_tools": [],
                "counts": {"skills": 0, "mcp_tools": 0},
                "total_matches": {"skills": 0, "mcp_tools": 0},
                "state": "no_match"
            })
        );
    }

    #[tokio::test]
    async fn capability_search_rediscovers_skills_between_steps_of_one_turn() {
        let gate = Gate::default();
        let server = FakeServer::start([
            Reply::sse(&chat_tool_call_events(
                "search-1",
                "capability_search",
                r#"{"query":"review"}"#,
            )),
            Reply::sse(&chat_tool_call_events(
                "search-2",
                "capability_search",
                r#"{"query":"late"}"#,
            ))
            .after(&gate),
            Reply::sse(&chat_text_events(&["done"])),
        ]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, "skills/review", "review");
        harness.submit("search twice");
        timeout(
            Duration::from_secs(10),
            harness.until(|event| matches!(event, UiEvent::ToolFinished { .. })),
        )
        .await
        .unwrap();
        write_skill(&harness.home, "skills/late", "late");
        gate.open();
        timeout(
            Duration::from_secs(10),
            harness.until(|event| matches!(event, UiEvent::TurnFinished { .. })),
        )
        .await
        .unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        let body = requests[2].json();
        let output = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_call_id"] == "search-2")
            .unwrap();
        let result: Value = serde_json::from_str(output["content"].as_str().unwrap()).unwrap();
        assert_eq!(result["skills"][0]["name"], "late");
        assert_eq!(result["counts"]["skills"], 1);
        assert!(!system_text(&requests[0].json()).contains("- late:"));
    }

    #[tokio::test]
    async fn capability_search_turn_obeys_existing_outer_cancellation() {
        let server = FakeServer::start([
            Reply::sse(&chat_tool_call_events(
                "search-1",
                "capability_search",
                r#"{"query":"review"}"#,
            )),
            Reply::held_sse(&chat_text_events(&["partial"])[..2]),
        ]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, "skills/review", "review");
        harness.submit("search and explain");
        let events = timeout(
            Duration::from_secs(10),
            harness.until(|event| matches!(event, UiEvent::AssistantText { .. })),
        )
        .await
        .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            UiEvent::ToolFinished {
                status: ToolResultStatus::Success,
                ..
            }
        )));
        harness.send(UiCommand::Cancel {
            turn_id: harness.running_turn(),
        });
        timeout(
            Duration::from_secs(10),
            harness.until(finished(TurnOutcome::Interrupted)),
        )
        .await
        .unwrap();
        assert_eq!(server.requests().len(), 2);
    }

    fn system_text(body: &Value) -> String {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "system")
            .map(|message| message["content"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    const SIGNED_OUT: &str = "Codex needs a subscription login. Run /login, open Connections, then choose Codex subscription.";

    async fn notices_of(harness: &mut Harness, command: &str) -> Vec<(NoticeTone, String, String)> {
        harness.command(command);
        harness.command("/version");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { notice } if notice.topic == "version"))
            .await;
        let mut shown = notices(shown);
        shown.pop();
        shown
    }

    fn auth(tone: NoticeTone, body: &str) -> (NoticeTone, String, String) {
        (tone, "auth".to_owned(), body.to_owned())
    }

    #[tokio::test]
    async fn signing_out_of_the_selected_codex_login_leaves_no_provider_to_use() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(false, 1);
        let mut harness = Harness::codex(&codex, &catalog).await;
        let login = harness.home.path().join("data/chatgpt-auth.json");
        assert_eq!(
            notices_of(&mut harness, "/logout codex").await,
            [
                auth(NoticeTone::Neutral, "Signed out of Codex."),
                (
                    NoticeTone::Warning,
                    "provider".to_owned(),
                    "No connected provider is available. Use /provider to sign in.".to_owned()
                ),
            ]
        );
        assert!(!login.exists());
        harness.submit("hello");
        let refused = harness
            .until(|event| matches!(event, UiEvent::Notice { notice } if notice.topic == "auth"))
            .await;
        assert_eq!(notices(refused), [auth(NoticeTone::Warning, SIGNED_OUT)]);
        harness.send(UiCommand::DropHeldPrompt);
        assert_eq!(
            notices_of(&mut harness, "/logout").await,
            [
                auth(NoticeTone::Neutral, "No Codex login session found."),
                (
                    NoticeTone::Warning,
                    "provider".to_owned(),
                    "No connected provider is available. Use /provider to sign in.".to_owned()
                ),
            ]
        );
        assert!(codex.requests().is_empty());
    }

    const SIGNED_IN_TOKEN: &str = "header.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF9zaGVsbCJ9LCJleHAiOjQxMDI0NDQ4MDB9.signature";

    fn signing_in_endpoints(
        auth: &FakeServer,
        codex: &FakeServer,
        catalog: &FakeServer,
    ) -> SubscriptionEndpoints {
        SubscriptionEndpoints {
            chatgpt: ofx_auth::ChatGptEndpoints {
                issuer: auth.base_url(),
                token_url: format!("{}/oauth/token", auth.base_url()),
                callback_ports: vec![0],
            },
            ..codex_endpoints(codex, catalog)
        }
    }

    fn granted_tokens() -> Reply {
        let tokens = json!({
            "access_token": SIGNED_IN_TOKEN,
            "refresh_token": "rt-shell",
            "expires_in": 3600,
        });
        Reply::status(200, tokens.to_string())
    }

    fn query_value(url: &str, key: &str) -> String {
        let query = url.split_once('?').unwrap().1;
        let raw = query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
            .unwrap();
        raw.replace("%3A", ":").replace("%2F", "/")
    }

    fn authorize_in_browser(url: &str) -> String {
        let redirect = query_value(url, "redirect_uri");
        let state = query_value(url, "state");
        let address = redirect.strip_prefix("http://").unwrap();
        let (host, path) = address.split_once('/').unwrap();
        let port: u16 = host.split_once(':').unwrap().1.parse().unwrap();
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let request =
            format!("GET /{path}?code=granted&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        std::io::Write::write_all(&mut stream, request.as_bytes()).unwrap();
        let mut response = String::new();
        let _ = std::io::Read::read_to_string(&mut stream, &mut response);
        response
    }

    fn sign_in_url(events: &[UiEvent]) -> String {
        events
            .iter()
            .find_map(|event| match event {
                UiEvent::SignInStarted { url } => Some(url.clone()),
                _ => None,
            })
            .unwrap()
    }

    fn sign_in_started(event: &UiEvent) -> bool {
        matches!(event, UiEvent::SignInStarted { .. })
    }

    async fn signing_in(
        settings: &Value,
        auth: &FakeServer,
        codex: &FakeServer,
        catalog: &FakeServer,
    ) -> Harness {
        let home = tempfile::tempdir().unwrap();
        let endpoints = signing_in_endpoints(auth, codex, catalog);
        let setup = agent_setup_with(&home, settings, endpoints).await;
        Harness::with_setup(home, setup)
    }

    #[tokio::test]
    async fn choosing_codex_without_a_login_signs_in_from_the_shell_then_switches() {
        let auth = FakeServer::start([granted_tokens()]);
        let codex = FakeServer::start([codex_text("signed in")]);
        let catalog = codex_catalog(false, 2);
        let local = FakeServer::start([]);
        let other = FakeServer::start([]);
        let settings = switching_settings("local", &local, &other);
        let mut harness = signing_in(&settings, &auth, &codex, &catalog).await;
        harness.send(select_provider("codex"));
        let started = within(harness.until(sign_in_started)).await;
        assert_eq!(
            notice_body(started),
            ["provider|Preparing Codex subscription."]
        );
        let url = sign_in_url(started);
        assert!(
            url.starts_with(&format!("{}/oauth/authorize?", auth.base_url())),
            "{url}"
        );
        let browser = std::thread::spawn(move || authorize_in_browser(&url));
        let switched = within(harness.until(provider_notice)).await;
        assert!(switched.contains(&UiEvent::SignInEnded));
        assert_eq!(
            notice_body(switched),
            [
                "provider|Preparing Codex subscription.",
                &format!("provider|Switched to Codex subscription with {CODEX_MODEL}."),
            ]
        );
        assert!(switched.contains(&UiEvent::ProviderSelected {
            provider: "codex".to_owned()
        }));
        assert!(browser.join().unwrap().starts_with("HTTP/1.1 200"));
        assert_eq!(saved_settings(&harness)["provider"], "codex");
        harness.submit("hello");
        within(harness.until(finished(TurnOutcome::Completed))).await;
        assert_eq!(codex.requests()[0].json()["model"], CODEX_MODEL);
    }

    #[tokio::test]
    async fn a_cancelled_shell_sign_in_saves_nothing_and_keeps_the_provider() {
        let auth = FakeServer::start([]);
        let codex = FakeServer::start([]);
        let catalog = FakeServer::start([]);
        let local = FakeServer::start([]);
        let other = FakeServer::start([]);
        let settings = switching_settings("local", &local, &other);
        let mut harness = signing_in(&settings, &auth, &codex, &catalog).await;
        harness.send(select_provider("codex"));
        within(harness.until(sign_in_started)).await;
        harness.send(UiCommand::CancelSignIn);
        within(harness.until(|event| *event == UiEvent::SignInEnded)).await;
        assert!(
            notices_of(&mut harness, "/model")
                .await
                .iter()
                .all(|(_, topic, _)| topic == "model")
        );
        assert!(!harness.home.path().join("data/chatgpt-auth.json").exists());
        assert_eq!(saved_settings(&harness)["provider"], "local");
        assert!(auth.requests().is_empty());
    }

    #[tokio::test]
    async fn prompts_sent_before_the_sign_in_screen_wait_behind_it_and_are_dropped_on_cancel() {
        let auth = FakeServer::start([]);
        let codex = FakeServer::start([]);
        let catalog = FakeServer::start([]);
        let local = FakeServer::start([Reply::held_sse(&chat_text_events(&["partial\n"])[..2])]);
        let other = FakeServer::start([]);
        let settings = switching_settings("local", &local, &other);
        let mut harness = signing_in(&settings, &auth, &codex, &catalog).await;
        harness.send(select_provider("codex"));
        harness.submit("first");
        harness.submit("second");
        within(harness.until(sign_in_started)).await;
        for _ in 0..2 {
            let held = within(harness.until(|event| *event == UiEvent::PromptHeld)).await;
            assert!(notice_body(held).is_empty());
        }
        harness.send(UiCommand::CancelSignIn);
        let dropped = within(harness.until(|event| *event == UiEvent::HeldPromptDropped)).await;
        assert!(dropped.contains(&UiEvent::SignInEnded));
        assert!(
            !dropped
                .iter()
                .any(|event| matches!(event, UiEvent::TurnStarted { .. }))
        );
        assert!(local.requests().is_empty());
        assert_eq!(saved_settings(&harness)["provider"], "local");
    }

    #[tokio::test]
    async fn prompts_sent_before_the_sign_in_screen_run_in_order_on_codex_once_signed_in() {
        let auth = FakeServer::start([granted_tokens()]);
        let codex = FakeServer::start([codex_text("one"), codex_text("two")]);
        let catalog = codex_catalog(false, 2);
        let local = FakeServer::start([]);
        let other = FakeServer::start([]);
        let settings = switching_settings("local", &local, &other);
        let mut harness = signing_in(&settings, &auth, &codex, &catalog).await;
        harness.send(select_provider("codex"));
        harness.submit("first");
        harness.submit("second");
        let started = within(harness.until(sign_in_started)).await;
        let url = sign_in_url(started);
        for _ in 0..2 {
            within(harness.until(|event| *event == UiEvent::PromptHeld)).await;
        }
        let browser = std::thread::spawn(move || authorize_in_browser(&url));
        for _ in 0..2 {
            within(harness.until(finished(TurnOutcome::Completed))).await;
        }
        browser.join().unwrap();
        assert!(local.requests().is_empty());
        let requests = codex.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].json()["model"], CODEX_MODEL);
        let first = requests[0].json().to_string();
        assert!(
            first.contains("first") && !first.contains("second"),
            "{first}"
        );
        assert!(requests[1].json().to_string().contains("second"));
    }

    #[tokio::test]
    async fn a_sign_in_cancelled_during_typed_ahead_compaction_ends_once_it_stops() {
        let auth = FakeServer::start([]);
        let codex = FakeServer::start([]);
        let catalog = FakeServer::start([]);
        let local = tool_work_then_chat(held_summary(), &[]);
        let other = FakeServer::start([]);
        let settings = switching_settings("local", &local, &other);
        let mut harness = signing_in(&settings, &auth, &codex, &catalog).await;
        chat(&mut harness, &["read the notes", "q1", "q2", "q3", "q4"]).await;
        harness.send(select_provider("codex"));
        harness.command("/compact");
        within(harness.until(sign_in_started)).await;
        summary_requested(&local).await;
        harness.send(UiCommand::CancelSignIn);
        harness.send(UiCommand::CancelCompaction);
        let ended = within(harness.until(|event| *event == UiEvent::SignInEnded)).await;
        assert_eq!(
            activities(ended).last(),
            Some(&CompactionActivity::Ended(CompactionEnd::Cancelled))
        );
        assert!(auth.requests().is_empty());
        assert_eq!(saved_settings(&harness)["provider"], "local");
    }

    #[tokio::test]
    async fn a_prompt_held_while_signed_out_runs_once_the_shell_signs_in() {
        let auth = FakeServer::start([granted_tokens()]);
        let codex = FakeServer::start([codex_text("welcome back")]);
        let catalog = codex_catalog(false, 2);
        let mut harness = signing_in(&codex_settings(), &auth, &codex, &catalog).await;
        held(&mut harness, "hello").await;
        harness.send(select_provider("codex"));
        let started = within(harness.until(sign_in_started)).await;
        let url = sign_in_url(started);
        let browser = std::thread::spawn(move || authorize_in_browser(&url));
        let resumed = within(harness.until(finished(TurnOutcome::Completed))).await;
        browser.join().unwrap();
        assert_eq!(
            notice_body(resumed),
            [
                "provider|Preparing Codex subscription.",
                &format!("provider|Switched to Codex subscription with {CODEX_MODEL}."),
            ]
        );
        assert!(resumed.contains(&UiEvent::LoginChanged { missing: false }));
        let request = codex.requests()[0].json();
        assert_eq!(request["model"], CODEX_MODEL);
        assert!(request.to_string().contains("hello"), "{request}");
    }

    #[tokio::test]
    async fn a_codex_login_rejected_mid_session_signs_in_again_from_the_login_column() {
        let auth = FakeServer::start([
            Reply::status(400, r#"{"error":{"code":"refresh_token_expired"}}"#),
            granted_tokens(),
        ]);
        let codex = FakeServer::start([
            Reply::status(401, r#"{"error":{"message":"token expired"}}"#),
            codex_text("welcome back"),
        ]);
        let catalog = codex_catalog(false, 4);
        let home = codex_home();
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        let endpoints = signing_in_endpoints(&auth, &codex, &catalog);
        let setup = agent_setup_with(&home, &settings, endpoints).await;
        let mut harness = Harness::with_setup(home, setup);
        harness.submit("hello");
        let failed = within(harness.until(finished(TurnOutcome::Failed))).await;
        assert!(failed.iter().any(|event| matches!(
            event,
            UiEvent::ApiStatus { text, .. }
                if text.ends_with("Reconnect Codex through /login to repair this source.")
        )));
        assert!(!harness.home.path().join("data/chatgpt-auth.json").exists());
        assert_eq!(
            switched(&mut harness, "codex").await,
            ["provider|Already using Codex subscription."]
        );
        harness.send(UiCommand::SignIn {
            provider: "codex".to_owned(),
        });
        let started = within(harness.until(sign_in_started)).await;
        assert!(notice_body(started).is_empty());
        let url = sign_in_url(started);
        let browser = std::thread::spawn(move || authorize_in_browser(&url));
        let signed_in = within(harness.until(provider_notice)).await;
        browser.join().unwrap();
        assert_eq!(
            notice_body(signed_in),
            [
                "provider|Preparing Codex subscription.",
                &format!("provider|Switched to Codex subscription with {CODEX_MODEL}."),
            ]
        );
        harness.submit("again");
        within(harness.until(finished(TurnOutcome::Completed))).await;
        let requests = codex.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].header("authorization"),
            Some(format!("Bearer {SIGNED_IN_TOKEN}").as_str())
        );
    }

    fn codex_message(call: &str, agent: &str, message: &str) -> Reply {
        let arguments = json!({
            "request": {
                "action": "message",
                "agent": agent,
                "instructions": "Answer briefly.",
                "message": message,
            }
        })
        .to_string();
        let events = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":call,"name":"subagent","arguments":""}}),
            json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":arguments}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}),
        ]
        .map(|event| event.to_string());
        Reply::sse(&events)
    }

    #[tokio::test]
    async fn a_child_started_before_a_logout_runs_on_the_login_the_shell_signs_in_with() {
        let auth = FakeServer::start([granted_tokens()]);
        let codex = FakeServer::start([
            codex_message("call_1", "helper", "first errand"),
            codex_text("child one"),
            codex_text("parent one"),
            codex_message("call_2", "helper", "second errand"),
            codex_text("child two"),
            codex_text("parent two"),
        ]);
        let catalog = codex_catalog(false, 4);
        let home = codex_home();
        let settings = json!({
            "provider": "codex",
            "models": {"codex": CODEX_MODEL},
            "session_titles": false
        });
        let setup = agent_setup_with(
            &home,
            &settings,
            signing_in_endpoints(&auth, &codex, &catalog),
        )
        .await;
        let mut harness = Harness::saved(home, setup);
        harness.submit("start the helper");
        within(harness.until(finished(TurnOutcome::Completed))).await;
        notices_of(&mut harness, "/logout codex").await;
        harness.send(select_provider("codex"));
        let started = within(harness.until(sign_in_started)).await;
        let url = sign_in_url(started);
        let browser = std::thread::spawn(move || authorize_in_browser(&url));
        within(harness.until(provider_notice)).await;
        browser.join().unwrap();
        harness.submit("ask the helper again");
        within(harness.until(finished(TurnOutcome::Completed))).await;
        let requests = codex.requests();
        let child_asked = |errand: &str| {
            requests.iter().find(|request| {
                let body = request.json();
                body["instructions"]
                    .as_str()
                    .is_some_and(|text| text.contains("Answer briefly."))
                    && body["input"].to_string().contains(errand)
            })
        };
        let first = child_asked("first errand").expect("the child's first request");
        assert_eq!(
            first.header("authorization"),
            Some(
                format!(
                    "Bearer {}",
                    "eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl"
                )
                .as_str()
            )
        );
        let second = child_asked("second errand").expect("the child's request after the sign-in");
        assert_eq!(
            second.header("authorization"),
            Some(format!("Bearer {SIGNED_IN_TOKEN}").as_str())
        );
        assert_eq!(requests.len(), 6);
    }

    #[tokio::test]
    async fn a_cancelled_sign_in_drops_the_held_prompt() {
        let auth = FakeServer::start([]);
        let codex = FakeServer::start([]);
        let catalog = FakeServer::start([]);
        let mut harness = signing_in(&codex_settings(), &auth, &codex, &catalog).await;
        held(&mut harness, "hello").await;
        harness.send(select_provider("codex"));
        within(harness.until(sign_in_started)).await;
        harness.send(UiCommand::CancelSignIn);
        let dropped = within(harness.until(|event| *event == UiEvent::HeldPromptDropped)).await;
        assert!(dropped.contains(&UiEvent::SignInEnded));
        assert!(notices(dropped).is_empty());
        assert!(codex.requests().is_empty());
    }

    async fn signed_out(codex: &FakeServer, catalog: &FakeServer) -> Harness {
        let home = tempfile::tempdir().unwrap();
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        let setup = agent_setup_with(&home, &settings, codex_endpoints(codex, catalog)).await;
        Harness::with_setup(home, setup)
    }

    async fn held(harness: &mut Harness, prompt: &str) {
        harness.submit(prompt);
        within(harness.until(|event| *event == UiEvent::PromptHeld)).await;
    }

    fn save_login(harness: &Harness) {
        let saved = codex_home();
        fs::create_dir_all(harness.home.path().join("data")).unwrap();
        fs::copy(
            saved.path().join("data/chatgpt-auth.json"),
            harness.home.path().join("data/chatgpt-auth.json"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_held_prompt_retried_without_a_login_asks_again_and_runs_once_one_is_saved() {
        let codex = FakeServer::start([codex_text("found it")]);
        let catalog = codex_catalog(false, 2);
        let mut harness = signed_out(&codex, &catalog).await;
        held(&mut harness, "hello").await;
        harness.send(UiCommand::RetryHeldPrompt);
        let asked =
            within(harness.until(
                |event| matches!(event, UiEvent::Notice { notice } if notice.topic == "auth"),
            ))
            .await;
        assert_eq!(notices(asked), [auth(NoticeTone::Warning, SIGNED_OUT)]);
        save_login(&harness);
        harness.send(UiCommand::RetryHeldPrompt);
        let ran = within(harness.until(finished(TurnOutcome::Completed))).await;
        assert!(notices(ran).is_empty());
        assert!(ran.contains(&UiEvent::LoginChanged { missing: false }));
        assert!(codex.requests()[0].json().to_string().contains("hello"));
    }

    #[tokio::test]
    async fn a_held_prompt_dropped_from_the_shell_never_runs() {
        let codex = FakeServer::start([codex_text("only the next one")]);
        let catalog = codex_catalog(false, 2);
        let mut harness = signed_out(&codex, &catalog).await;
        held(&mut harness, "dropped").await;
        harness.send(UiCommand::DropHeldPrompt);
        save_login(&harness);
        harness.send(UiCommand::RetryHeldPrompt);
        within(harness.until(|event| *event == UiEvent::LoginChanged { missing: false })).await;
        harness.submit("next");
        within(harness.until(finished(TurnOutcome::Completed))).await;
        let request = codex.requests()[0].json().to_string();
        assert!(request.contains("next"), "{request}");
        assert!(!request.contains("dropped"), "{request}");
    }

    #[tokio::test]
    async fn compacting_while_signed_out_asks_for_authentication() {
        let codex = FakeServer::start([codex_text("one")]);
        let catalog = codex_catalog(false, 1);
        let mut harness = Harness::codex(&codex, &catalog).await;
        chat(&mut harness, &["one"]).await;
        notices_of(&mut harness, "/logout codex").await;
        harness.command("/compact");
        let ended = within(harness.until(compaction_settled)).await;
        assert_eq!(
            activities(ended),
            [CompactionActivity::Ended(
                CompactionEnd::AuthenticationRejected
            )]
        );
        held(&mut harness, "two").await;
        harness.command("/compact");
        let ended = within(harness.until(compaction_settled)).await;
        assert_eq!(
            activities(ended),
            [CompactionActivity::Ended(CompactionEnd::Busy)]
        );
    }

    #[tokio::test]
    async fn resuming_a_session_while_signed_out_asks_for_a_login() {
        let codex = FakeServer::start([codex_text("one")]);
        let catalog = codex_catalog(false, 1);
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        let mut harness = Harness::codex_saved(&codex, &catalog, &settings).await;
        chat(&mut harness, &["first question"]).await;
        let first = saved_sessions(&harness.home)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        notices_of(&mut harness, "/logout codex").await;
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        harness.send(UiCommand::ResumeSession { id: first });
        harness
            .until(|event| matches!(event, UiEvent::SessionResumed { .. }))
            .await;
        let shown =
            within(harness.until(
                |event| matches!(event, UiEvent::Notice { notice } if notice.topic == "auth"),
            ))
            .await;
        assert_eq!(
            notices(shown).last(),
            Some(&auth(NoticeTone::Warning, SIGNED_OUT))
        );
    }

    #[tokio::test]
    async fn signing_out_drops_the_catalog_that_carried_the_login() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(false, 2);
        let mut harness = Harness::codex(&codex, &catalog).await;
        notices_of(&mut harness, "/logout codex").await;
        let asked = catalog.requests().len();
        assert_eq!(
            listed_catalog(&mut harness).await,
            ModelCatalog::Failed { retry: None }
        );
        assert_eq!(catalog.requests().len(), asked);
        assert!(codex.requests().is_empty());
    }

    #[tokio::test]
    async fn signing_out_stops_a_title_request_still_running_on_the_removed_login() {
        let title_held = Gate::default();
        let listing_held = Gate::default();
        let codex = FakeServer::start([
            codex_text("Fix the renderer").after(&title_held),
            codex_text("done"),
        ]);
        let catalog = FakeServer::start([
            catalog_version(),
            catalog_listing(false).after(&listing_held),
            catalog_listing(false),
        ]);
        let mut settings = codex_settings();
        settings["effort"] = json!("low");
        let mut harness = Harness::codex_saved(&codex, &catalog, &settings).await;
        harness.submit("please fix the renderer");
        within(async {
            while codex.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert_eq!(title_requests(&codex).len(), 1);
        listing_held.open();
        within(harness.until(finished(TurnOutcome::Completed))).await;
        notices_of(&mut harness, "/logout codex").await;
        title_held.open();
        let named = timeout(
            Duration::from_millis(500),
            harness.until(titled(Some("Fix the renderer"))),
        )
        .await;
        assert!(named.is_err(), "{:?}", harness.seen);
        assert_eq!(
            saved_sessions(&harness.home)[0]["title"],
            "please fix the renderer"
        );
        assert_eq!(codex.requests().len(), 2);
    }

    #[tokio::test]
    async fn clearing_the_conversation_drops_prompts_held_while_signed_out() {
        for command in ["/clear", "/new", "/reset"] {
            let codex = FakeServer::start([codex_text("only the next one")]);
            let catalog = codex_catalog(false, 2);
            let mut harness = signed_out(&codex, &catalog).await;
            held(&mut harness, "discard me").await;
            harness.command(command);
            within(harness.until(|event| matches!(event, UiEvent::ConversationCleared { .. })))
                .await;
            save_login(&harness);
            harness.send(UiCommand::RetryHeldPrompt);
            within(harness.until(|event| *event == UiEvent::LoginChanged { missing: false })).await;
            harness.submit("next");
            within(harness.until(finished(TurnOutcome::Completed))).await;
            let request = codex.requests()[0].json().to_string();
            assert!(request.contains("next"), "{command}: {request}");
            assert!(!request.contains("discard me"), "{command}: {request}");
        }
    }

    fn save_login_as(harness: &Harness, account: &str) {
        let data = harness.home.path().join("data");
        fs::create_dir_all(&data).unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        let session = json!({
            "version": 1,
            "access_token": "eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl",
            "refresh_token": "rt-refresh-secret-0123456789",
            "expires_at_ms": 4_102_444_800_000_i64,
            "account_id": account,
        });
        let file = data.join("chatgpt-auth.json");
        fs::write(&file, format!("{session}\n")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn rejected_after_a_retry() -> FakeServer {
        FakeServer::start([
            Reply::status(429, r#"{"error":{"message":"slow down"}}"#),
            Reply::status(400, r#"{"error":{"message":"bad"}}"#),
        ])
    }

    async fn checkpoint_identity(harness: &mut Harness, account: &str) -> Value {
        save_login_as(harness, account);
        harness.send(UiCommand::RetryHeldPrompt);
        within(harness.until(finished(TurnOutcome::Failed))).await;
        let id = saved_sessions(&harness.home)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let recovery = harness
            .home
            .path()
            .join("data/sessions")
            .join(id)
            .join("recovery.json");
        let saved: Value = serde_json::from_slice(&fs::read(recovery).unwrap()).unwrap();
        saved["checkpoint"]["authority"]["credential_identity"].clone()
    }

    #[tokio::test]
    async fn a_restored_login_signs_the_recovery_checkpoints_of_the_prompts_it_releases() {
        let codex = rejected_after_a_retry();
        let catalog = codex_catalog(false, 2);
        let home = tempfile::tempdir().unwrap();
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        let setup = agent_setup_with(&home, &settings, codex_endpoints(&codex, &catalog)).await;
        let mut harness = Harness::saved(home, setup);
        held(&mut harness, "first").await;
        let launched_signed_out = checkpoint_identity(&mut harness, "acct_test").await;
        assert!(launched_signed_out.is_string(), "{launched_signed_out}");

        let codex = rejected_after_a_retry();
        let catalog = codex_catalog(false, 3);
        let mut harness = Harness::codex_saved(&codex, &catalog, &settings).await;
        notices_of(&mut harness, "/logout codex").await;
        held(&mut harness, "second").await;
        let changed_account = checkpoint_identity(&mut harness, "acct_other").await;
        assert!(changed_account.is_string(), "{changed_account}");
        assert_ne!(changed_account, launched_signed_out);
    }

    #[tokio::test]
    async fn a_restored_login_keeps_the_session_s_model() {
        let codex = FakeServer::start([codex_text("kept")]);
        let catalog = codex_catalog(false, 2);
        let mut harness = signed_out(&codex, &catalog).await;
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        settings["models"]["codex"] = json!(OTHER_CODEX_MODEL);
        fs::write(
            harness.home.path().join("config/settings.json"),
            settings.to_string(),
        )
        .unwrap();
        held(&mut harness, "hello").await;
        save_login(&harness);
        harness.send(UiCommand::RetryHeldPrompt);
        let ran = within(harness.until(finished(TurnOutcome::Completed))).await;
        assert!(
            !ran.iter()
                .any(|event| matches!(event, UiEvent::ModelSelected { .. })),
            "{ran:?}"
        );
        assert_eq!(codex.requests()[0].json()["model"], CODEX_MODEL);
    }

    fn continues_nothing(shown: &[UiEvent]) {
        assert!(
            !shown.iter().any(|event| matches!(
                event,
                UiEvent::RecoveryContinuing { .. }
                    | UiEvent::TurnStarted { .. }
                    | UiEvent::PromptHeld
            )),
            "{shown:?}"
        );
    }

    fn rebind_notice(saved: &str) -> (NoticeTone, String, String) {
        (
            NoticeTone::Warning,
            "session".to_owned(),
            format!(
                "This session was saved with the {saved} provider, which oh-fx cannot use yet; it continues with codex."
            ),
        )
    }

    #[tokio::test]
    async fn a_session_saved_with_a_provider_oh_fx_cannot_use_continues_with_the_current_one() {
        for (provider, label) in [
            (json!("gateway"), "gateway"),
            (
                json!({"name": "fx-only", "binding": "ab".repeat(32)}),
                "fx-only",
            ),
        ] {
            let codex = FakeServer::start([codex_text("first answer")]);
            let catalog = codex_catalog(false, 2);
            let mut settings = codex_settings();
            settings["session_titles"] = json!(false);
            let mut harness = Harness::codex_saved(&codex, &catalog, &settings).await;
            chat(&mut harness, &["first question"]).await;
            let id = saved_sessions(&harness.home)[0]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let home = harness.finish().await;
            let manifest = home
                .path()
                .join("data/sessions")
                .join(&id)
                .join("session.json");
            let mut metadata: Value =
                serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
            metadata["provider"] = provider;
            metadata["model"] = json!("openai/gpt-5");
            metadata["effort"] = json!("low");
            fs::write(&manifest, metadata.to_string()).unwrap();

            let mut harness =
                Harness::resuming(home, &settings, codex_endpoints(&codex, &catalog), &id).await;
            let shown = notices_of(&mut harness, "/status").await;
            assert!(shown.contains(&rebind_notice(label)), "{label}: {shown:?}");
            let saved: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
            assert_eq!(saved["provider"], "codex", "{label}");
            assert_eq!(saved["model"], CODEX_MODEL, "{label}");
            assert_eq!(saved["effort"], "low", "{label}");
        }
    }

    const FX_ID: &str = "fx0123456789";

    fn save_in_fx(home: &std::path::Path, prompts: &[&str]) -> std::path::PathBuf {
        let fx = home.join(".fx");
        let session = fx.join("sessions").join(FX_ID);
        fs::create_dir_all(&session).unwrap();
        for directory in [&fx, &fx.join("sessions"), &session] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let manifest = format!(
            "{{\"schema_version\":4,\"id\":\"{FX_ID}\",\"origin_workspace_root\":\"/elsewhere/fx-work\",\"workspace_root\":\"/elsewhere/fx-work\",\"created_at_ms\":1,\"updated_at_ms\":2,\"conversation_language\":\"en\",\"provider\":\"gateway\",\"model\":\"openai/gpt-5\",\"effort\":\"auto\",\"fast_mode\":false,\"title\":\"Started in fx\",\"subagent_child\":false}}"
        );
        let events: String = (0_u64..)
            .zip(prompts)
            .flat_map(|(turn, prompt)| {
                [
                    json!({"user": {"text": prompt, "images": [], "work_id": null}}),
                    json!({"assistant": {"text": "done in fx", "provider_replay": null, "standalone_response": false}}),
                    json!({"turn_completed": {"files": [], "turn_summary": null}}),
                ]
                .into_iter()
                .zip(1_u64..)
                .map(move |(event, offset)| {
                    let frame = json!({"schema_version": 3, "seq": turn * 3 + offset, "timestamp_ms": 2, "event": event});
                    format!("{frame}\n")
                })
            })
            .collect();
        for (name, bytes) in [
            ("session.json", manifest),
            ("events.jsonl", events),
            ("session.lock", String::new()),
        ] {
            fs::write(session.join(name), bytes).unwrap();
            fs::set_permissions(session.join(name), fs::Permissions::from_mode(0o600)).unwrap();
        }
        session
    }

    async fn listed_rows(harness: &mut Harness, scope: SessionScope) -> Vec<(String, bool)> {
        harness.send(UiCommand::ListSessions {
            scope,
            after: None,
            limit: 10,
        });
        let events = harness
            .until(|event| matches!(event, UiEvent::SessionsListed { .. }))
            .await;
        let Some(UiEvent::SessionsListed { page }) = events.last() else {
            panic!("the picker lists a page");
        };
        page.rows
            .iter()
            .map(|row| (row.id.clone(), row.from_fx))
            .collect()
    }

    async fn picked(harness: &mut Harness, id: &str) -> Vec<UiEvent> {
        harness.send(UiCommand::ResumeSession { id: id.to_owned() });
        harness
            .until(|event| {
                matches!(
                    event,
                    UiEvent::SessionResumeFailed { .. } | UiEvent::SessionResumed { .. }
                )
            })
            .await
            .to_vec()
    }

    #[tokio::test]
    async fn the_picker_lists_an_fx_session_and_enter_imports_and_resumes_it() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(false, 4);
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        let home = codex_home();
        let fx = save_in_fx(home.path(), &["asked in fx"]);
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(fx.join("session.json")).unwrap()).unwrap();
        manifest["ultrafast_mode"] = json!(true);
        fs::write(fx.join("session.json"), manifest.to_string()).unwrap();
        let untouched = fs::read(fx.join("events.jsonl")).unwrap();
        let mut harness = Harness::codex_saved_in(home, &codex, &catalog, &settings).await;

        assert_eq!(
            listed_rows(&mut harness, SessionScope::AllWorkspaces).await,
            [(FX_ID.to_owned(), true)]
        );
        let switched = picked(&mut harness, FX_ID).await;
        let Some(UiEvent::SessionResumed { history }) = switched.last() else {
            panic!("the fx session resumes: {switched:?}");
        };
        assert!(
            format!("{history:?}").contains("asked in fx"),
            "{history:?}"
        );
        let shown = notices_of(&mut harness, "/status").await;
        assert!(shown.contains(&rebind_notice("gateway")), "{shown:?}");
        assert_eq!(
            ultrafast_notice(&mut harness, "/ultrafast").await,
            (NoticeTone::Neutral, "requested: on".to_owned())
        );
        let copy = harness.home.path().join("data/sessions").join(FX_ID);
        let saved: Value =
            serde_json::from_slice(&fs::read(copy.join("session.json")).unwrap()).unwrap();
        assert_eq!(saved["provider"], "codex");
        assert_eq!(saved["model"], CODEX_MODEL);
        assert_eq!(saved["ultrafast_mode"], true);
        let marker: Value =
            serde_json::from_slice(&fs::read(copy.join("fx-import.json")).unwrap()).unwrap();
        assert_eq!(marker["copy"]["preferences"]["provider"], "codex");
        assert_eq!(marker["copy"]["preferences"]["ultrafast_mode"], true);
        assert_eq!(
            fs::read(copy.join("events.jsonl")).unwrap(),
            fs::read(fx.join("events.jsonl")).unwrap()
        );
        assert_eq!(fs::read(fx.join("events.jsonl")).unwrap(), untouched);
    }

    #[tokio::test]
    async fn the_picker_refuses_an_fx_session_that_fx_has_open() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(false, 4);
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        let home = codex_home();
        let fx = save_in_fx(home.path(), &["asked in fx"]);
        let lock = fs::File::open(fx.join("session.lock")).unwrap();
        lock.lock().unwrap();
        let mut harness = Harness::codex_saved_in(home, &codex, &catalog, &settings).await;

        let switched = picked(&mut harness, FX_ID).await;
        assert!(
            matches!(
                switched.last(),
                Some(UiEvent::SessionResumeFailed {
                    refusal: ResumeRefusal::Unavailable,
                    ..
                })
            ),
            "{switched:?}"
        );
        assert!(
            notices(&switched).contains(&(
                NoticeTone::Warning,
                "session".to_owned(),
                "fx has this session open; close it in fx, then resume it here".to_owned()
            )),
            "{switched:?}"
        );
        assert!(
            !harness
                .home
                .path()
                .join("data/sessions")
                .join(FX_ID)
                .exists()
        );
    }

    #[tokio::test]
    async fn an_fx_session_follows_fx_until_its_model_is_changed_in_the_shell() {
        let codex = FakeServer::start([]);
        let catalog = codex_catalog(false, 8);
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        let endpoints = || codex_endpoints(&codex, &catalog);
        let home = codex_home();
        let fx = save_in_fx(home.path(), &["asked in fx"]);
        let copy = home.path().join("data/sessions").join(FX_ID);
        let saved = || -> Value {
            serde_json::from_slice(&fs::read(copy.join("session.json")).unwrap()).unwrap()
        };

        let mut harness = Harness::resuming(home, &settings, endpoints(), FX_ID).await;
        let shown = notices_of(&mut harness, "/status").await;
        assert!(shown.contains(&rebind_notice("gateway")), "{shown:?}");
        let home = harness.finish().await;
        assert_eq!(saved()["provider"], "codex");

        save_in_fx(home.path(), &["asked in fx", "followed up in fx"]);
        let mut harness = Harness::resuming(home, &settings, endpoints(), FX_ID).await;
        let shown = notices_of(&mut harness, &format!("/model {OTHER_CODEX_MODEL}")).await;
        assert!(shown.contains(&rebind_notice("gateway")), "{shown:?}");
        assert_eq!(
            fs::read(copy.join("events.jsonl")).unwrap(),
            fs::read(fx.join("events.jsonl")).unwrap()
        );
        let home = harness.finish().await;
        assert_eq!(saved()["model"], OTHER_CODEX_MODEL);

        save_in_fx(
            home.path(),
            &["asked in fx", "followed up in fx", "asked again in fx"],
        );
        let mut harness = Harness::resuming(home, &settings, endpoints(), FX_ID).await;
        let shown = notices_of(&mut harness, "/status").await;
        assert!(!shown.contains(&rebind_notice("gateway")), "{shown:?}");
        let _home = harness.finish().await;
        let log = fs::read_to_string(copy.join("events.jsonl")).unwrap();
        assert!(log.contains("followed up in fx"), "{log}");
        assert!(!log.contains("asked again in fx"), "{log}");
        assert_eq!(saved()["provider"], "codex");
        assert_eq!(saved()["model"], OTHER_CODEX_MODEL);
    }

    #[tokio::test]
    async fn a_paused_response_reopened_signed_out_at_launch_waits_for_a_sign_in() {
        let codex = FakeServer::start([codex_text("first answer")]);
        let catalog = codex_catalog(false, 2);
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        let mut harness = Harness::codex_saved(&codex, &catalog, &settings).await;
        chat(&mut harness, &["first question"]).await;
        let id = saved_sessions(&harness.home)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let home = harness.finish().await;
        pause_a_saved_response(&home, &id);
        fs::remove_file(home.path().join("data/chatgpt-auth.json")).unwrap();
        let mut harness =
            Harness::resuming(home, &settings, codex_endpoints(&codex, &catalog), &id).await;
        let shown = notices_of(&mut harness, "/status").await;
        assert!(
            shown.contains(&(
                NoticeTone::Warning,
                "recovery".to_owned(),
                SIGN_IN_TO_CONTINUE.to_owned()
            )),
            "{shown:?}"
        );
        continues_nothing(&harness.seen);
        let recovery = harness
            .home
            .path()
            .join("data/sessions")
            .join(&id)
            .join("recovery.json");
        assert!(recovery.exists());
        assert_eq!(codex.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_paused_response_resumed_signed_out_from_the_picker_waits_for_a_sign_in() {
        let codex = FakeServer::start([codex_text("first answer")]);
        let catalog = codex_catalog(false, 2);
        let mut settings = codex_settings();
        settings["session_titles"] = json!(false);
        let mut harness = Harness::codex_saved(&codex, &catalog, &settings).await;
        chat(&mut harness, &["first question"]).await;
        let first = saved_sessions(&harness.home)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        notices_of(&mut harness, "/logout codex").await;
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        pause_a_saved_response(&harness.home, &first);
        let resumed_from = harness.seen.len();
        harness.send(UiCommand::ResumeSession { id: first.clone() });
        harness
            .until(|event| matches!(event, UiEvent::SessionResumed { .. }))
            .await;
        let shown = notices_of(&mut harness, "/status").await;
        assert!(
            shown.contains(&(
                NoticeTone::Warning,
                "recovery".to_owned(),
                SIGN_IN_TO_CONTINUE.to_owned()
            )),
            "{shown:?}"
        );
        continues_nothing(&harness.seen[resumed_from..]);
        let recovery = harness
            .home
            .path()
            .join("data/sessions")
            .join(&first)
            .join("recovery.json");
        assert!(recovery.exists());
        assert_eq!(codex.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_codex_launch_without_a_login_opens_signed_out() {
        let codex = FakeServer::start([]);
        let catalog = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let setup =
            agent_setup_with(&home, &codex_settings(), codex_endpoints(&codex, &catalog)).await;
        let mut harness = Harness::with_setup(home, setup);
        harness.submit("hello");
        let refused = harness
            .until(|event| matches!(event, UiEvent::Notice { notice } if notice.topic == "auth"))
            .await;
        assert_eq!(notices(refused), [auth(NoticeTone::Warning, SIGNED_OUT)]);
        assert_eq!(
            notices_of(&mut harness, "/model").await,
            [(
                NoticeTone::Neutral,
                "model".to_owned(),
                CODEX_MODEL.to_owned()
            )]
        );
        assert!(codex.requests().is_empty());
        assert!(catalog.requests().is_empty());
    }

    #[tokio::test]
    async fn logout_reports_each_provider_and_keeps_a_configured_one_in_use() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["still here"]))]);
        let home = codex_home();
        let setup = agent_setup_with(
            &home,
            &local_settings(&server),
            SubscriptionEndpoints::default(),
        )
        .await;
        let mut harness = Harness::with_setup(home, setup);
        assert_eq!(
            notices_of(&mut harness, "/logout").await,
            [auth(NoticeTone::Neutral, "Signed out of Codex.")]
        );
        for (command, body) in [
            ("/logout", "No oh-fx login session found."),
            ("/logout VERCEL", "No oh-fx login session found."),
            ("/logout codex", "No Codex login session found."),
            ("/logout grok", "No Grok login session found."),
        ] {
            assert_eq!(
                notices_of(&mut harness, command).await,
                [auth(NoticeTone::Neutral, body)],
                "{command}"
            );
        }
        assert_eq!(
            notices_of(&mut harness, "/logout chatgpt").await,
            [(
                NoticeTone::Warning,
                String::new(),
                "usage: /logout [vercel|codex|grok]".to_owned()
            )]
        );
        chat(&mut harness, &["hi"]).await;
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn the_selected_codex_login_stays_during_work_while_others_sign_out() {
        let codex = FakeServer::start([codex_partial()]);
        let catalog = codex_catalog(false, 1);
        let mut harness = Harness::codex(&codex, &catalog).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        assert_eq!(
            notices_of(&mut harness, "/logout codex").await,
            [auth(
                NoticeTone::Warning,
                "Sign out is unavailable until active and queued work finishes."
            )]
        );
        harness.command("/logout grok");
        let signed_out = within(harness.until(|event| {
            matches!(event, UiEvent::Notice { notice } if notice.body == "No Grok login session found.")
        }))
        .await;
        assert_eq!(
            notices(signed_out),
            [auth(NoticeTone::Neutral, "No Grok login session found.")]
        );
        assert!(harness.home.path().join("data/chatgpt-auth.json").exists());
        assert!(
            !harness
                .seen
                .iter()
                .any(|event| matches!(event, UiEvent::TurnFinished { .. }))
        );
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
    }

    fn notices(events: &[UiEvent]) -> Vec<(NoticeTone, String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                UiEvent::Notice { notice } => {
                    Some((notice.tone, notice.topic.clone(), notice.body.clone()))
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn each_prompt_rediscovers_skills_and_reports_the_skills_it_loaded() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["one"])),
            Reply::sse(&chat_text_events(&["two"])),
            Reply::sse(&chat_text_events(&["three"])),
        ]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, ".oh-fx/skills/review", "review");
        harness.submit("$review the diff");
        let first = harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(
            notices(first),
            [(
                NoticeTone::Neutral,
                String::new(),
                "1 requested skill loaded\n\u{2514} Loaded skill review".to_owned()
            )]
        );
        write_skill(&harness.home, "skills/late", "late");
        write_skill(&harness.home, ".claude/skills/review", "review");
        harness.submit("$late and $review");
        let second = harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(
            notices(second),
            [(
                NoticeTone::Warning,
                String::new(),
                "Requested skills \u{b7} 1 loaded \u{b7} 1 failed (ctrl+o for details)\n\u{251c} Loaded skill late\n\u{2514} Could not load review: ambiguous name".to_owned()
            )]
        );
        harness.submit("plain question");
        let third = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(notices(third).is_empty());
        let requests = server.requests();
        let first_system = system_text(&requests[0].json());
        assert!(first_system.contains("- review: review workflow (location: skill:"));
        assert!(!first_system.contains("- late:"));
        assert!(first_system.contains("<skill_content name=\"review\""));
        let second_system = system_text(&requests[1].json());
        assert!(second_system.contains("- late: late workflow (location: skill:"));
        assert!(second_system.contains("<skill_content name=\"late\""));
        assert!(second_system.contains(
            "\"review\" is ambiguous. Retry with the name and one advertised location: "
        ));
        let third_system = system_text(&requests[2].json());
        assert!(third_system.contains("<available_skills>"));
        assert!(!third_system.contains("Explicitly invoked skill content"));
    }

    #[tokio::test]
    async fn skipped_skills_warn_once_per_conversation_as_context_notices() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["one"])),
            Reply::sse(&chat_text_events(&["two"])),
            Reply::sse(&chat_text_events(&["three"])),
        ]);
        let mut harness = Harness::start(&server).await;
        let broken = fs::canonicalize(harness.home.path())
            .unwrap()
            .join("workspace/skills/broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("SKILL.md"), "---\ndescription: nameless\n---\n").unwrap();
        harness.submit("one");
        let first = harness.until(finished(TurnOutcome::Completed)).await;
        let warned = notices(first);
        assert_eq!(warned.len(), 1);
        assert_eq!(warned[0].0, NoticeTone::Warning);
        assert_eq!(warned[0].1, "context");
        assert!(
            warned[0].2.starts_with(&format!(
                "skill discovery warning: candidate \"{}\" was skipped",
                broken.display()
            )),
            "{warned:?}"
        );
        harness.submit("two");
        let second = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(notices(second).is_empty());
        harness.command("/clear");
        harness.submit("three");
        let third = harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(notices(third), warned);
    }

    fn is_skills_menu(event: &UiEvent) -> bool {
        matches!(event, UiEvent::SkillsMenu { .. })
    }

    fn is_notice(event: &UiEvent) -> bool {
        matches!(event, UiEvent::Notice { .. })
    }

    async fn skills_menu(harness: &mut Harness, command: &str) -> (Vec<String>, SkillMenuFocus) {
        harness.command(command);
        let events = harness.until(is_skills_menu).await;
        let Some(UiEvent::SkillsMenu { items, focus }) = events.last() else {
            unreachable!();
        };
        let names = items
            .iter()
            .map(|item| format!("{} {}", item.name, item.scope))
            .collect();
        (names, focus.clone())
    }

    async fn skills_notice(harness: &mut Harness, command: &str) -> String {
        harness.command(command);
        notice_body(harness.until(is_notice).await).join("\n")
    }

    #[tokio::test]
    async fn skills_commands_open_the_menu_on_the_rediscovered_catalog() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, ".oh-fx/skills/review", "review");
        write_skill(&harness.home, ".claude/skills/deploy", "deploy");
        assert_eq!(
            skills_menu(&mut harness, "/skills").await,
            (
                vec![
                    "review oh-fx \u{b7} Workspace".to_owned(),
                    "deploy Claude \u{b7} Workspace".to_owned()
                ],
                SkillMenuFocus::Start
            )
        );
        assert_eq!(
            skills_menu(&mut harness, "/skills show deploy").await.1,
            SkillMenuFocus::Item(1)
        );
        write_skill(&harness.home, "skills/deploy", "deploy");
        assert_eq!(
            skills_menu(&mut harness, "/skills show deploy").await.1,
            SkillMenuFocus::Query("deploy".to_owned())
        );
        assert_eq!(
            skills_notice(&mut harness, "/skills show missing").await,
            "skills|Skill 'missing' not found."
        );
    }

    fn skill_warnings(events: &[UiEvent]) -> usize {
        notice_body(events)
            .iter()
            .filter(|body| body.starts_with("skills|skill discovery warning:"))
            .count()
    }

    #[tokio::test]
    async fn skills_commands_warn_about_an_unchanged_invalid_skill_once_per_conversation() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        let broken = harness.home.path().join("workspace/skills/broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("SKILL.md"), "---\ndescription: nameless\n---\n").unwrap();
        harness.command("/skills");
        assert_eq!(skill_warnings(harness.until(is_skills_menu).await), 1);
        harness.command("/skills");
        assert_eq!(skill_warnings(harness.until(is_skills_menu).await), 0);
        harness.command("/clear");
        harness.command("/skills");
        assert_eq!(skill_warnings(harness.until(is_skills_menu).await), 1);
    }

    async fn skill_install_controller(
        server: &FakeServer,
    ) -> (tempfile::TempDir, Controller, Arc<Mutex<Vec<UiEvent>>>) {
        let home = tempfile::tempdir().unwrap();
        let setup = agent_setup(&home, server).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let emit: Emit = Arc::new(move |event| captured.lock().unwrap().push(event));
        let worker = Arc::new(WorkerRuntime::default());
        (
            home,
            Controller::new(setup, emit, None, false, worker),
            events,
        )
    }

    async fn direct_skill_install(controller: &mut Controller, text: &str) {
        let (_sender, mut commands) = unbounded_channel();
        assert!(controller.run_idle_command(text, &mut commands).await);
        assert!(controller.installation.is_some());
        finish_install(&controller.state, &mut controller.installation).await;
        controller.settle_deferred_commands(true).await;
    }

    fn held_install_lock(
        home: &tempfile::TempDir,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        held_install_lock_for(home, "install-pack")
    }

    fn held_install_lock_for(
        home: &tempfile::TempDir,
        name: &str,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let locks = home.path().join("config/.skill-install-locks");
        fs::create_dir_all(&locks).unwrap();
        fs::set_permissions(&locks, fs::Permissions::from_mode(0o700)).unwrap();
        let file = fs::File::create(locks.join(name)).unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(10));
            drop(file);
        });
        (release, worker)
    }

    fn pause_a_saved_response(home: &tempfile::TempDir, id: &str) {
        let dir = home.path().join("data/sessions").join(id);
        let metadata: Value =
            serde_json::from_slice(&fs::read(dir.join("session.json")).unwrap()).unwrap();
        let seq = fs::read_to_string(dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .count();
        let checkpoint = json!({
            "version": 2,
            "turn_id": 2,
            "user": {"text": "fix the build", "images": []},
            "assistant_source": "",
            "execution": {
                "schema_version": 10,
                "tool_steps": [],
                "files": [],
                "steering": [],
                "turn_summary": null
            },
            "cause": "rate_limited",
            "action": "retrying_request",
            "tool_state": "none",
            "authority": {
                "provider": metadata["provider"],
                "model": metadata["model"],
                "credential_source": null,
                "credential_identity": null
            },
            "requested_fast_mode": false,
            "fast_mode": false,
            "max_provider_attempts": 10,
            "consumed_provider_attempts": 0,
            "outstanding_reservation": false
        });
        fs::write(
            dir.join("recovery.json"),
            format!("{{\"conversation_seq\":{seq},\"checkpoint\":{checkpoint}}}\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_recovery_waiting_for_an_installation_keeps_its_session_until_it_runs() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["answer a"])),
            Reply::sse(&chat_text_events(&["answer b"])),
            Reply::sse(&chat_text_events(&["build fixed"])),
        ]);
        let home = tempfile::tempdir().unwrap();
        let setup = agent_setup(&home, &server).await;
        write_skill(&home, "install-pack", "new-skill");
        let source = fs::canonicalize(home.path().join("workspace/install-pack")).unwrap();
        let mut harness = Harness::saved(home, setup);
        chat(&mut harness, &["question a"]).await;
        let first = saved_sessions(&harness.home)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        chat(&mut harness, &["question b"]).await;
        let second = saved_sessions(&harness.home)
            .iter()
            .filter_map(|session| session["id"].as_str())
            .find(|id| *id != first)
            .unwrap()
            .to_owned();
        pause_a_saved_response(&harness.home, &first);
        let (release, worker) = held_install_lock(&harness.home);
        harness.command(&format!("/skills install {}", source.display()));
        harness.send(UiCommand::ResumeSession { id: first.clone() });
        harness
            .until(|event| matches!(event, UiEvent::RecoveryContinuing { .. }))
            .await;
        harness.command("/resume");
        harness.send(UiCommand::ResumeSession { id: second.clone() });
        let switched = harness
            .until(|event| {
                matches!(
                    event,
                    UiEvent::SessionResumeFailed { .. } | UiEvent::SessionResumed { .. }
                )
            })
            .await;
        assert!(
            matches!(
                switched.last(),
                Some(UiEvent::SessionResumeFailed { id, .. }) if *id == second
            ),
            "{switched:?}"
        );
        assert!(
            !switched
                .iter()
                .any(|event| matches!(event, UiEvent::SessionPickerOpened { .. })),
            "{switched:?}"
        );
        release.send(()).unwrap();
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        let continued = requests[2].body_text();
        assert!(continued.contains("question a"), "{continued}");
        assert!(continued.contains("fix the build"), "{continued}");
        assert!(!continued.contains("question b"), "{continued}");
        let saved = fs::read_to_string(
            harness
                .home
                .path()
                .join("data/sessions")
                .join(&first)
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(saved.contains("build fixed"), "{saved}");
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn pending_install_keeps_active_turn_progress_and_cancellation_live() {
        let gate = Gate::default();
        let held = Reply::held_sse(&chat_text_events(&["during installation\n"])[..2]).after(&gate);
        let server = FakeServer::start([held, Reply::sse(&chat_text_events(&["next"]))]);
        let home = tempfile::tempdir().unwrap();
        let setup = agent_setup(&home, &server).await;
        write_skill(&home, "install-pack", "new-skill");
        let source = fs::canonicalize(home.path().join("workspace/install-pack")).unwrap();
        let (release, worker) = held_install_lock(&home);
        let mut harness = Harness::with_setup_observer(home, setup, move |event| {
            if matches!(event, UiEvent::StatsRequested) {
                gate.open();
            }
            if matches!(
                event,
                UiEvent::TurnFinished {
                    outcome: TurnOutcome::Interrupted,
                    ..
                }
            ) {
                let _ = release.send(());
            }
        });
        harness.submit("active");
        harness
            .until(|event| matches!(event, UiEvent::TurnStarted { .. }))
            .await;
        harness.command(&format!("/skills install {}", source.display()));
        harness.command("/stats");
        let during = harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        assert!(
            notices(during).is_empty(),
            "installation ran before the active operation settled: {during:?}"
        );
        let turn_id = harness.running_turn();
        harness.submit("$new-skill next prompt");
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
        let installed = harness.until(|event| matches!(event, UiEvent::Notice { notice } if notice.body == "Installed: new-skill")).await;
        assert_eq!(
            notice_body(installed),
            [
                format!("skills|Installing from {}...", source.display()),
                "skills|Installed: new-skill".to_owned()
            ]
        );
        harness.until(finished(TurnOutcome::Completed)).await;
        assert!(
            system_text(&server.requests()[1].json()).contains("<skill_content name=\"new-skill\"")
        );
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn pending_installs_preserve_fifo_and_compaction_cancellation() {
        let server = tool_work_then_chat(held_summary(), &["next"]);
        let home = tempfile::tempdir().unwrap();
        let setup = agent_setup(&home, &server).await;
        write_skill(&home, "install-pack", "new-skill");
        write_skill(&home, "second-pack", "second-skill");
        let first = fs::canonicalize(home.path().join("workspace/install-pack")).unwrap();
        let second = fs::canonicalize(home.path().join("workspace/second-pack")).unwrap();
        let (release, worker) = held_install_lock(&home);
        let mut harness = Harness::with_setup_observer(home, setup, move |event| {
            if matches!(
                event,
                UiEvent::CompactionActivity {
                    activity: CompactionActivity::Ended(CompactionEnd::Cancelled)
                }
            ) {
                let _ = release.send(());
            }
        });
        chat(&mut harness, &["read the notes", "q1", "q2", "q3", "q4"]).await;
        harness.command("/compact");
        harness
            .until(|event| {
                matches!(
                    event,
                    UiEvent::CompactionActivity {
                        activity: CompactionActivity::Summarizing
                    }
                )
            })
            .await;
        summary_requested(&server).await;
        harness.command(&format!("/skills install {}", first.display()));
        harness.command(&format!("/skills add {}", second.display()));
        harness.command("/stats");
        let during = harness
            .until(|event| matches!(event, UiEvent::StatsRequested))
            .await;
        assert!(
            notices(during).is_empty(),
            "installation ran during compaction: {during:?}"
        );
        harness.submit("$new-skill and $second-skill next prompt");
        harness.send(UiCommand::CancelCompaction);
        assert_eq!(
            activities(harness.until(compaction_settled).await),
            [CompactionActivity::Ended(CompactionEnd::Cancelled)]
        );
        let installed = harness.until(|event| matches!(event, UiEvent::Notice { notice } if notice.body == "Installed: second-skill")).await;
        assert_eq!(
            notice_body(installed),
            [
                format!("skills|Installing from {}...", first.display()),
                "skills|Installed: new-skill".to_owned(),
                format!("skills|Installing from {}...", second.display()),
                "skills|Installed: second-skill".to_owned(),
            ]
        );
        harness.until(finished(TurnOutcome::Completed)).await;
        let request = system_text(&server.requests()[7].json());
        assert!(request.contains("<skill_content name=\"new-skill\""));
        assert!(request.contains("<skill_content name=\"second-skill\""));
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn skill_install_and_show_complete_before_a_held_turn_is_released() {
        let server = FakeServer::start([Reply::held_sse(&chat_text_events(&["partial"])[..2])]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, "install-pack", "new-skill");
        let source = fs::canonicalize(harness.home.path().join("workspace/install-pack")).unwrap();
        harness.submit("keep streaming");
        timeout(
            Duration::from_secs(10),
            harness.until(|event| matches!(event, UiEvent::AssistantText { .. })),
        )
        .await
        .unwrap();
        harness.command(&format!("/skills install {}", source.display()));
        harness.command("/skills show new-skill");
        let shown = timeout(
            Duration::from_secs(10),
            harness.until(|event| {
                is_skills_menu(event)
                    || matches!(event, UiEvent::Notice { notice }
                if notice.body == "Skill 'new-skill' not found.")
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            notice_body(shown),
            [
                format!("skills|Installing from {}...", source.display()),
                "skills|Installed: new-skill".to_owned(),
            ]
        );
        let Some(UiEvent::SkillsMenu { items, focus }) = shown.last() else {
            panic!("show did not open the installed skill menu: {shown:?}");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "new-skill");
        assert_eq!(*focus, SkillMenuFocus::Item(0));
        assert!(
            !shown
                .iter()
                .any(|event| matches!(event, UiEvent::TurnFinished { .. }))
        );
        let notices: Vec<_> = shown
            .iter()
            .enumerate()
            .filter_map(|(index, event)| matches!(event, UiEvent::Notice { .. }).then_some(index))
            .collect();
        assert_eq!(notices[1], notices[0] + 1);
        assert_eq!(server.requests().len(), 1);
        harness.send(UiCommand::Cancel {
            turn_id: harness.running_turn(),
        });
        timeout(
            Duration::from_secs(10),
            harness.until(finished(TurnOutcome::Interrupted)),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn clear_retains_language_of_a_prompt_waiting_for_the_next_install() {
        let server = FakeServer::start([
            Reply::held_sse(&chat_text_events(&["active"])[..2]),
            Reply::sse(&chat_text_events(&["Готово."])),
        ]);
        let mut harness = Harness::start_saved(&server).await;
        write_skill(&harness.home, "first-pack", "first-skill");
        write_skill(&harness.home, "install-pack", "second-skill");
        let first = fs::canonicalize(harness.home.path().join("workspace/first-pack")).unwrap();
        let second = fs::canonicalize(harness.home.path().join("workspace/install-pack")).unwrap();
        let (release_first, worker_first) = held_install_lock_for(&harness.home, "first-pack");
        let (release_second, worker_second) = held_install_lock(&harness.home);
        harness.submit("active");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        harness.command(&format!("/skills install {}", first.display()));
        harness.command("/clear");
        harness.command(&format!("/skills install {}", second.display()));
        harness.submit("Открой файл");
        harness.command("/stats");
        harness
            .until(|event| matches!(event, UiEvent::StatsRequested))
            .await;
        release_first.send(()).unwrap();
        timeout(
            Duration::from_secs(10),
            harness.until(|event| {
                matches!(
                    event,
                    UiEvent::ConversationCleared {
                        first_kept_prompt: 1
                    }
                )
            }),
        )
        .await
        .unwrap();
        assert_eq!(server.requests().len(), 1);
        assert!(
            !harness
                .home
                .path()
                .join("config/skills/install-pack")
                .exists()
        );
        release_second.send(()).unwrap();
        timeout(
            Duration::from_secs(10),
            harness.until(finished(TurnOutcome::Completed)),
        )
        .await
        .unwrap();
        let sessions = saved_sessions(&harness.home);
        let fresh = sessions
            .iter()
            .find(|session| session["title"] == "Открой файл")
            .unwrap_or_else(|| panic!("fresh prompt session absent: {sessions:?}"));
        assert_eq!(fresh["conversation_language"], "und-Cyrl");
        assert_eq!(server.requests().len(), 2);
        worker_first.join().unwrap();
        worker_second.join().unwrap();
    }

    #[tokio::test]
    async fn completed_install_reopens_skill_commands_and_clear_drops_earlier_prompts() {
        let server = FakeServer::start([
            outside_read(),
            Reply::sse(&chat_text_events(&["queued prompt must not run"])),
        ]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, "install-pack", "new-skill");
        let source = fs::canonicalize(harness.home.path().join("workspace/install-pack")).unwrap();
        fs::write(harness.home.path().join("outside.txt"), "notes\n").unwrap();
        harness.submit("active");
        harness.until(approval_requested).await;
        harness.submit("queued prompt");
        harness.command(&format!("/skills install {}", source.display()));
        timeout(Duration::from_secs(10), harness.until(|event| matches!(event, UiEvent::Notice { notice } if notice.body == "Installed: new-skill"))).await.unwrap();
        harness.command("/skills show new-skill");
        harness.command("/stats");
        let shown = timeout(
            Duration::from_secs(10),
            harness.until(|event| matches!(event, UiEvent::StatsRequested)),
        )
        .await
        .unwrap();
        assert!(shown.iter().any(|event| matches!(event, UiEvent::SkillsMenu { items, focus: SkillMenuFocus::Item(0) } if items.len() == 1 && items[0].name == "new-skill")), "completed install still holds skill commands: {shown:?}");
        harness.command("/clear");
        timeout(
            Duration::from_secs(10),
            harness.until(finished(TurnOutcome::Interrupted)),
        )
        .await
        .unwrap();
        harness.command("/stats");
        let settled = harness
            .until(|event| matches!(event, UiEvent::StatsRequested))
            .await;
        assert!(settled.iter().any(|event| matches!(
            event,
            UiEvent::ConversationCleared {
                first_kept_prompt: 2
            }
        )));
        assert_eq!(server.requests().len(), 1);
    }

    async fn assert_install_completion_order(starts_idle: bool) {
        let mut replies = Vec::new();
        if !starts_idle {
            replies.push(outside_read());
        }
        replies.push(Reply::sse(&chat_text_events(&["after clear"])));
        let server = FakeServer::start(replies);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, "first-pack", "first-skill");
        write_skill(&harness.home, "second-pack", "second-skill");
        let first = fs::canonicalize(harness.home.path().join("workspace/first-pack")).unwrap();
        let second = fs::canonicalize(harness.home.path().join("workspace/second-pack")).unwrap();
        let (release, worker) = held_install_lock(&harness.home);
        let first_locked = harness.home.path().join("workspace/install-pack");
        fs::rename(&first, &first_locked).unwrap();
        let first_locked = fs::canonicalize(first_locked).unwrap();
        if !starts_idle {
            fs::write(harness.home.path().join("outside.txt"), "notes\n").unwrap();
            harness.submit("active");
            harness.until(approval_requested).await;
        }
        harness.command(&format!("/skills install {}", first_locked.display()));
        harness.submit("queued prompt");
        harness.command(&format!("/skills install {}", second.display()));
        harness.command("/clear");
        harness.submit("$second-skill after clear");
        harness.command("/stats");
        harness
            .until(|event| matches!(event, UiEvent::StatsRequested))
            .await;
        release.send(()).unwrap();
        let first_done = harness.until(|event| matches!(event, UiEvent::Notice { notice } if notice.body == "Installed: first-skill")).await;
        assert_eq!(notice_body(first_done).len(), 2);
        let final_events = harness.until(finished(TurnOutcome::Completed)).await;
        let installed = final_events.iter().position(|event| matches!(event, UiEvent::Notice { notice } if notice.body == "Installed: second-skill")).unwrap();
        let cleared = final_events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    UiEvent::ConversationCleared { first_kept_prompt } if *first_kept_prompt == if starts_idle { 1 } else { 2 }
                )
            })
            .unwrap();
        assert!(installed < cleared);
        assert!(!server.requests().iter().any(|request| {
            request.json()["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["content"] == "queued prompt")
        }));
        let body = server.requests()[usize::from(!starts_idle)].json();
        assert!(system_text(&body).contains("<skill_content name=\"second-skill\""));
        assert!(
            !body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["content"] == "queued prompt")
        );
        assert_eq!(server.requests().len(), if starts_idle { 1 } else { 2 });

        if !starts_idle {
            assert!(final_events.iter().any(|event| matches!(
                event,
                UiEvent::TurnFinished {
                    outcome: TurnOutcome::Interrupted,
                    ..
                }
            )));
        }
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn busy_install_completion_preserves_prompt_install_and_clear_arrival_order() {
        timeout(
            Duration::from_secs(10),
            assert_install_completion_order(false),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn idle_install_completion_preserves_prompt_install_and_clear_arrival_order() {
        timeout(
            Duration::from_secs(10),
            assert_install_completion_order(true),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_install_join_emits_adjacent_notices_and_releases_shutdown_guard() {
        let server = FakeServer::start([]);
        let (_home, mut controller, events) = skill_install_controller(&server).await;
        controller.installation = Some(crate::skill_commands::panicking_install(
            &controller.state,
            "panicking-pack",
        ));
        assert!(
            controller
                .state
                .skills()
                .installations()
                .wait_for_running(Duration::ZERO)
        );
        finish_install(&controller.state, &mut controller.installation).await;
        assert_eq!(
            notice_body(&events.lock().unwrap()),
            [
                "skills|Installing from panicking-pack...",
                "skills|Failed to install. Check the source path or URL and try again."
            ]
        );
        assert!(controller.installation.is_none());
        assert!(
            !controller
                .state
                .skills()
                .installations()
                .wait_for_running(Duration::ZERO)
        );
    }

    #[tokio::test]
    async fn local_skill_install_reports_metadata_names_and_refreshes_before_returning() {
        let server = FakeServer::start([]);
        let (home, mut controller, events) = skill_install_controller(&server).await;
        write_skill(&home, "install-pack", "root-skill");
        write_skill(&home, "install-pack/review", "parsed-review");
        let source = fs::canonicalize(home.path().join("workspace/install-pack")).unwrap();
        assert!(controller.state.skills().current().skills.is_empty());
        direct_skill_install(
            &mut controller,
            &format!("/skills install {}", source.display()),
        )
        .await;
        assert_eq!(
            notices(&events.lock().unwrap()),
            [
                (
                    NoticeTone::Neutral,
                    "skills".to_owned(),
                    format!("Installing from {}...", source.display())
                ),
                (
                    NoticeTone::Neutral,
                    "skills".to_owned(),
                    "Installed: root-skill\nInstalled: parsed-review".to_owned()
                ),
            ]
        );
        let mut names: Vec<_> = controller
            .state
            .skills()
            .current()
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, ["parsed-review", "root-skill"]);
        assert!(
            controller
                .state
                .skills()
                .managed_root()
                .join("install-pack/SKILL.md")
                .is_file()
        );
        assert!(
            controller
                .state
                .skills()
                .managed_root()
                .join("review/SKILL.md")
                .is_file()
        );
    }

    #[tokio::test]
    async fn local_skill_install_filters_aliases_and_supplies_the_next_prompt() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, "install-pack", "root-skill");
        write_skill(&harness.home, "install-pack/review", "parsed-review");
        let source = fs::canonicalize(harness.home.path().join("workspace/install-pack")).unwrap();
        harness.command(&format!("/skills add {} --skill=review", source.display()));
        assert_eq!(
            notice_body(harness.until(is_notice).await),
            [format!("skills|Installing from {}...", source.display())]
        );
        assert_eq!(
            notice_body(harness.until(is_notice).await),
            ["skills|Installed: parsed-review"]
        );
        let managed = fs::canonicalize(harness.home.path().join("config"))
            .unwrap()
            .join("skills");
        assert!(!managed.join("install-pack").exists());
        assert!(managed.join("review/SKILL.md").is_file());
        for filter in ["--skill parsed-review", "--skill=", "--skill=\t "] {
            harness.command(&format!("/skills install {} {filter}", source.display()));
            assert_eq!(
                notice_body(harness.until(is_notice).await),
                [format!("skills|Installing from {}...", source.display())]
            );
            let expected = if filter == "--skill parsed-review" {
                "skills|Installed: parsed-review"
            } else {
                "skills|Installed: root-skill\nInstalled: parsed-review"
            };
            assert_eq!(notice_body(harness.until(is_notice).await), [expected]);
        }
        harness.submit("$parsed-review use this workflow");
        let shown = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(
            notices(shown)
                .iter()
                .any(|(_, _, body)| body.contains("Loaded skill parsed-review"))
        );
        let request = server.requests().remove(0).json();
        assert!(system_text(&request).contains("<skill_content name=\"parsed-review\""));
    }

    #[tokio::test]
    async fn local_skill_install_preserves_empty_filtered_and_failure_notices() {
        let server = FakeServer::start([]);
        let (home, mut controller, events) = skill_install_controller(&server).await;
        let empty = home.path().join("workspace/empty-pack");
        fs::create_dir_all(&empty).unwrap();
        let empty = fs::canonicalize(empty).unwrap();
        for (arguments, expected) in [
            (
                format!("install {}", empty.display()),
                "No skills found (no SKILL.md files).",
            ),
            (
                format!("add {} --skill missing", empty.display()),
                "Skill 'missing' not found in the repository.",
            ),
            (
                format!("install {}/missing", empty.display()),
                "Failed to install. Check the source path or URL and try again.",
            ),
        ] {
            events.lock().unwrap().clear();
            direct_skill_install(&mut controller, &format!("/skills {arguments}")).await;
            let source = arguments
                .strip_prefix("install ")
                .or_else(|| arguments.strip_prefix("add "))
                .unwrap()
                .split(" --skill")
                .next()
                .unwrap();
            assert_eq!(
                notices(&events.lock().unwrap()),
                [
                    (
                        NoticeTone::Neutral,
                        "skills".to_owned(),
                        format!("Installing from {source}...")
                    ),
                    (
                        NoticeTone::Neutral,
                        "skills".to_owned(),
                        expected.to_owned()
                    ),
                ]
            );
            assert!(controller.state.skills().current().skills.is_empty());
            assert!(
                !controller
                    .state
                    .skills()
                    .installations()
                    .wait_for_running(Duration::ZERO)
            );
        }
    }

    #[tokio::test]
    async fn local_skill_install_rejects_a_linked_managed_root_without_outside_writes() {
        let server = FakeServer::start([]);
        let (home, mut controller, events) = skill_install_controller(&server).await;
        write_skill(&home, "install-pack", "root-skill");
        let source = fs::canonicalize(home.path().join("workspace/install-pack")).unwrap();
        let outside = home.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, controller.state.skills().managed_root()).unwrap();
        direct_skill_install(
            &mut controller,
            &format!("/skills install {}", source.display()),
        )
        .await;
        assert_eq!(
            notices(&events.lock().unwrap()),
            [
                (
                    NoticeTone::Neutral,
                    "skills".to_owned(),
                    format!("Installing from {}...", source.display())
                ),
                (
                    NoticeTone::Neutral,
                    "skills".to_owned(),
                    "Failed to install. Check the source path or URL and try again.".to_owned()
                ),
            ]
        );
        assert_eq!(
            fs::read_to_string(outside.join("sentinel")).unwrap(),
            "unchanged"
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        assert!(controller.state.skills().current().skills.is_empty());
    }

    #[tokio::test]
    async fn skills_commands_create_and_remove_only_inside_the_managed_root() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        let managed = fs::canonicalize(harness.home.path().join("config"))
            .unwrap()
            .join("skills");
        let created = managed.join("fresh/SKILL.md");
        assert_eq!(
            skills_notice(&mut harness, "/skills create fresh").await,
            format!("skills|Created {}", created.display())
        );
        assert!(created.is_file());
        assert_eq!(
            skills_menu(&mut harness, "/skills").await.0,
            ["fresh oh-fx \u{b7} Global"]
        );
        assert_eq!(
            skills_notice(&mut harness, "/skills remove fresh").await,
            "skills|Removed skill 'fresh'."
        );
        assert!(!managed.join("fresh").exists());
        write_skill(&harness.home, ".oh-fx/skills/review", "review");
        let review = fs::canonicalize(harness.home.path())
            .unwrap()
            .join("workspace/.oh-fx/skills/review");
        assert_eq!(
            skills_notice(&mut harness, "/skills remove review").await,
            format!(
                "skills|Skill 'review' comes from workspace .oh-fx/skills, not the oh-fx managed install root. Remove it from {}.",
                review.display()
            )
        );
        assert!(review.exists());
        assert_eq!(
            skills_notice(&mut harness, "/skills create a/b").await,
            "skills|Invalid skill name. Use a single directory name without '/' or '\\'."
        );
        assert_eq!(
            skills_notice(&mut harness, "/skills path").await,
            format!(
                "skills|oh-fx workspace roots are auto-discovered from .oh-fx/skills and skills/.\noh-fx managed install root: {}\ncompatibility roots are auto-discovered from workspace and home (.opencode/.codex/.claude/.agents/.claw).",
                managed.display()
            )
        );
        assert_eq!(
            skills_notice(&mut harness, "/skills list all").await,
            "|usage: /skills [list|add|install|show|create|remove|path] [name|url|path]"
        );
        harness.command("/skills add vercel-labs/agent-skills --skill review");
        harness.until(is_notice).await;
        assert_eq!(
            notice_body(harness.until(is_notice).await),
            ["skills|Failed to install. Check the source path or URL and try again."]
        );
    }

    #[tokio::test]
    async fn a_skill_bound_in_the_composer_picks_one_of_two_same_named_skills() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
        let mut harness = Harness::start(&server).await;
        write_skill(&harness.home, ".oh-fx/skills/review", "review");
        write_skill(&harness.home, ".claude/skills/review", "review");
        let claude = fs::canonicalize(harness.home.path())
            .unwrap()
            .join("workspace/.claude/skills/review");
        harness.send(UiCommand::Submit {
            prompt: "$review the diff".to_owned(),
            skills: vec![SkillBinding {
                name: "review".to_owned(),
                path: claude.clone(),
            }],
        });
        let events = harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(
            notices(events),
            [(
                NoticeTone::Neutral,
                String::new(),
                "1 requested skill loaded\n\u{2514} Loaded skill review".to_owned()
            )]
        );
        let system = system_text(&server.requests()[0].json());
        assert!(
            system.contains(&format!(
                "<skill_content name=\"review\" location=\"{}\" resource=\"SKILL.md\"",
                claude.display()
            )),
            "{system}"
        );
    }

    fn delegate(task: &str) -> Reply {
        Reply::sse(&chat_tool_call_events(
            "call-delegate",
            "subagent",
            &json!({"request": {"action": "run", "task": task}}).to_string(),
        ))
    }

    fn read_outside() -> Reply {
        Reply::sse(&chat_tool_call_events(
            "call-read",
            "read_file",
            r#"{"path":"../outside.txt"}"#,
        ))
    }

    fn approval_requests(events: &[UiEvent]) -> Vec<(TurnId, ApprovalRequest)> {
        events
            .iter()
            .filter_map(|event| match event {
                UiEvent::ApprovalRequested { turn_id, request } => {
                    Some((*turn_id, (**request).clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn last_tool_result(body: &Value) -> String {
        let messages = body["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "tool", "{last}");
        last["content"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn only_a_session_that_is_saved_offers_subagent() {
        for saved in [false, true] {
            let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
            let mut harness = if saved {
                Harness::start_saved(&server).await
            } else {
                Harness::start(&server).await
            };
            harness.submit("hi");
            harness.until(finished(TurnOutcome::Completed)).await;
            let offered = server.requests()[0].json()["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["function"]["name"] == "subagent");
            assert_eq!(offered, saved);
        }
    }

    fn message_reader(call_id: &str, message: &str) -> Reply {
        Reply::sse(&chat_tool_call_events(
            call_id,
            "subagent",
            &json!({"request": {"action": "message", "agent": "reader", "message": message}})
                .to_string(),
        ))
    }

    async fn approve_next(harness: &mut Harness, decision: ApprovalDecision) -> ApprovalRequest {
        let requested = harness
            .until(|event| matches!(event, UiEvent::ApprovalRequested { .. }))
            .await
            .to_vec();
        let [(_, request)] = approval_requests(&requested).try_into().unwrap();
        harness.send(UiCommand::Approval {
            request_id: request.id,
            answer: decision.into(),
        });
        request
    }

    #[tokio::test]
    async fn a_session_resumed_from_the_picker_continues_its_own_named_children() {
        let server = FakeServer::start([
            message_reader("call-first", "remember the number 7"),
            Reply::sse(&chat_text_events(&["noted"])),
            Reply::sse(&chat_text_events(&["parent done"])),
            message_reader("call-again", "what was the number"),
            Reply::sse(&chat_text_events(&["it was 7"])),
            Reply::sse(&chat_text_events(&["parent done again"])),
        ]);
        let mut harness = Harness::start_saved(&server).await;
        chat(&mut harness, &["ask the reader to remember"]).await;
        let parent = saved_sessions(&harness.home)
            .into_iter()
            .find(|saved| saved["subagent_child"] != true)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        harness.send(UiCommand::ResumeSession { id: parent });
        harness
            .until(|event| matches!(event, UiEvent::SessionResumed { .. }))
            .await;
        chat(&mut harness, &["ask the reader again"]).await;
        let requests = server.requests();
        assert_eq!(requests.len(), 6);
        assert_eq!(user_messages(&requests[4].json()), 2);
        assert!(requests[4].body_text().contains("remember the number 7"));
        assert!(requests[4].body_text().contains("noted"));
        assert_eq!(
            last_tool_result(&requests[5].json()),
            r#"{"ok":true,"result":"it was 7","error_code":null}"#
        );
    }

    #[tokio::test]
    async fn a_resumed_session_never_reaches_the_children_of_the_session_it_left() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["one"])),
            message_reader("call-first", "read the notes"),
            read_outside(),
            Reply::sse(&chat_text_events(&["the notes say hi"])),
            Reply::sse(&chat_text_events(&["parent done"])),
            message_reader("call-again", "read them again"),
            read_outside(),
            Reply::sse(&chat_text_events(&["still hi"])),
            Reply::sse(&chat_text_events(&["parent done again"])),
        ]);
        let mut harness = Harness::start_saved(&server).await;
        fs::write(harness.home.path().join("outside.txt"), "hi\n").unwrap();
        chat(&mut harness, &["first question"]).await;
        let earlier = saved_sessions(&harness.home)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        harness.command("/new");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        harness.submit("ask the reader");
        approve_next(&mut harness, ApprovalDecision::Always).await;
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.send(UiCommand::ResumeSession { id: earlier });
        harness
            .until(|event| matches!(event, UiEvent::SessionResumed { .. }))
            .await;
        harness.submit("ask the reader again");
        let request = approve_next(&mut harness, ApprovalDecision::Once).await;
        assert_eq!(request.tool_name, "read_file");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = server.requests();
        assert_eq!(requests.len(), 9);
        assert_eq!(user_messages(&requests[6].json()), 1);
        assert!(requests[6].body_text().contains("read them again"));
        assert!(!requests[6].body_text().contains("read the notes"));
    }

    #[tokio::test]
    async fn a_childs_approval_is_asked_under_the_parents_turn_and_its_grant_stays_with_the_child()
    {
        let server = FakeServer::start([
            delegate("read the notes"),
            read_outside(),
            Reply::sse(&chat_text_events(&["the notes say hi"])),
            Reply::sse(&chat_text_events(&["parent done"])),
        ]);
        let mut harness = Harness::start_saved(&server).await;
        fs::write(harness.home.path().join("outside.txt"), "hi\n").unwrap();
        harness.submit("delegate the reading");
        let requested = harness
            .until(|event| matches!(event, UiEvent::ApprovalRequested { .. }))
            .await
            .to_vec();
        let turn = harness.running_turn();
        let [(turn_id, request)] = approval_requests(&requested).try_into().unwrap();
        assert_eq!(turn_id, turn);
        let child = saved_sessions(&harness.home)
            .into_iter()
            .find(|saved| saved["subagent_child"] == true)
            .unwrap();
        assert_eq!(
            request.origin,
            ApprovalOrigin::Subagent(child["id"].as_str().unwrap().to_owned())
        );
        assert_eq!(request.tool_name, "read_file");
        let parent = saved_sessions(&harness.home)
            .into_iter()
            .find(|saved| saved["subagent_child"] != true)
            .unwrap();
        let registry: Value = serde_json::from_slice(
            &fs::read(
                harness
                    .home
                    .path()
                    .join("data/sessions")
                    .join(parent["id"].as_str().unwrap())
                    .join("subagent/children.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(registry["children"][0]["phase"], "awaiting_approval");
        harness.send(UiCommand::Approval {
            request_id: request.id,
            answer: ApprovalDecision::Always.into(),
        });
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert!(last_tool_result(&requests[2].json()).contains("hi"));
        assert_eq!(
            last_tool_result(&requests[3].json()),
            r#"{"ok":true,"result":"the notes say hi","error_code":null}"#
        );
        let status = status_notice(&mut harness).await;
        assert!(
            status.contains("\nsession_permission_grants=0\n"),
            "{status}"
        );
    }

    #[tokio::test]
    async fn children_follow_the_sessions_model_and_its_remembered_grants() {
        let server = FakeServer::start([
            read_outside(),
            delegate("read the notes again"),
            read_outside(),
            Reply::sse(&chat_text_events(&["read again"])),
            Reply::sse(&chat_text_events(&["parent done"])),
        ]);
        let mut harness = Harness::start_saved(&server).await;
        fs::write(harness.home.path().join("outside.txt"), "hi\n").unwrap();
        harness.command("/model model-b");
        harness
            .until(|event| matches!(event, UiEvent::ModelSelected { .. }))
            .await;
        harness.submit("read the notes, then delegate");
        let requested = harness
            .until(|event| matches!(event, UiEvent::ApprovalRequested { .. }))
            .await
            .to_vec();
        let [(_, request)] = approval_requests(&requested).try_into().unwrap();
        assert_eq!(request.origin, ApprovalOrigin::ActiveSession);
        harness.send(UiCommand::Approval {
            request_id: request.id,
            answer: ApprovalDecision::Always.into(),
        });
        let rest = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(approval_requests(rest).is_empty());
        let requests = server.requests();
        assert_eq!(requests.len(), 5);
        assert!(last_tool_result(&requests[3].json()).contains("hi"));
        assert!(
            requests
                .iter()
                .all(|request| request.json()["model"] == "vendor/model-b")
        );
    }

    #[tokio::test]
    async fn a_working_child_reports_the_sessions_model_before_its_row_settles() {
        let server = FakeServer::start([
            delegate("read the notes"),
            Reply::sse(&chat_text_events(&["the notes say hi"])),
            Reply::sse(&chat_text_events(&["parent done"])),
        ]);
        let mut harness = Harness::start_saved(&server).await;
        harness.submit("delegate the reading");
        let events = harness.until(finished(TurnOutcome::Completed)).await;
        let reported: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                UiEvent::SubagentStatus {
                    call_id, status, ..
                } => Some(format!("status {} {}", call_id.as_str(), status.model)),
                UiEvent::ToolFinished { call_id, .. } => {
                    Some(format!("finish {}", call_id.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            reported,
            ["status call-delegate model-a", "finish call-delegate"]
        );
    }

    #[tokio::test]
    async fn undo_leaves_a_childs_file_changes_alone() {
        let server = FakeServer::start([
            delegate("write the note"),
            write_call("call-write", "child.md", "from the child\n"),
            Reply::sse(&chat_text_events(&["wrote it"])),
            Reply::sse(&chat_text_events(&["parent done"])),
        ]);
        let mut harness = Harness::start_saved(&server).await;
        let workspace = fs::canonicalize(harness.home.path().join("workspace")).unwrap();
        harness.submit("delegate the note");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(
            fs::read_to_string(workspace.join("child.md")).unwrap(),
            "from the child\n"
        );
        assert_eq!(undo_notice(&mut harness).await, "undo|Nothing to undo.");
        assert!(workspace.join("child.md").exists());
    }

    #[test]
    fn failure_status_names_provider_and_model_errors_but_not_step_limits() {
        let mut error = ProviderError::new(ProviderErrorKind::ConnectionFailed, "ConnectionFailed");
        error.detail = Some("refused".to_owned());
        assert_eq!(
            failure_status(&TurnFailure::Provider(error), CredentialSource::Configured).unwrap(),
            "⚠ request failed: ConnectionFailed · refused"
        );
        let mut unauthorized = ProviderError::new(ProviderErrorKind::Unauthorized, "HttpError");
        unauthorized.status = Some(401);
        assert_eq!(
            failure_status(
                &TurnFailure::Provider(unauthorized),
                CredentialSource::Codex
            )
            .unwrap(),
            "⚠ Codex subscription authentication failed · HTTP 401 · Reconnect Codex through /login to repair this source."
        );
        assert_eq!(
            failure_status(&TurnFailure::InvalidCompletion, CredentialSource::Codex).unwrap(),
            "⚠ ModelError"
        );
        for silent in [
            TurnFailure::StepLimitReached,
            TurnFailure::RepeatedMalformedArguments,
            TurnFailure::RecoveryPaused,
            TurnFailure::RepeatedShellExecutionFailure,
        ] {
            assert_eq!(failure_status(&silent, CredentialSource::Configured), None);
        }
    }

    const QUESTION_ARGUMENTS: &str = r#"{"questions":[{"question":"Proceed?","options":[{"label":"Yes","description":"Go ahead"},{"label":"No"}]}]}"#;

    async fn asked(harness: &mut Harness) -> (TurnId, QuestionRequest) {
        let Some(UiEvent::QuestionRequested { turn_id, request }) = harness
            .until(|event| matches!(event, UiEvent::QuestionRequested { .. }))
            .await
            .last()
            .cloned()
        else {
            unreachable!()
        };
        (turn_id, request)
    }

    fn tool_results(body: &Value) -> Vec<String> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["content"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn questions_reach_the_shell_for_the_running_turn_and_answers_reach_the_model() {
        let ask = chat_tool_call_events("call-1", "ask_user_question", QUESTION_ARGUMENTS);
        let server = FakeServer::start([
            Reply::sse(&ask),
            Reply::sse(&chat_text_events(&["Stopping."])),
        ]);
        let mut harness = Harness::start(&server).await;
        harness.submit("pick for me");
        let (turn_id, request) = asked(&mut harness).await;
        assert_eq!(turn_id, harness.running_turn());
        assert_eq!(request.entries.len(), 1);
        assert_eq!(request.entries[0].question, "Proceed?");
        assert_eq!(request.entries[0].options.len(), 2);
        assert_eq!(
            request.entries[0].options[0].description.as_deref(),
            Some("Go ahead")
        );
        let offered: Vec<String> = server.requests()[0].json()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            offered.iter().any(|name| name == "ask_user_question"),
            "{offered:?}"
        );
        harness.send(UiCommand::QuestionAnswered {
            request_id: request.id,
            answers: Some(vec!["No".to_owned()]),
        });
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(
            tool_results(&server.requests()[1].json()),
            [r#"[{"question":"Proceed?","answer":"No"}]"#]
        );
    }

    #[tokio::test]
    async fn a_cancelled_turn_stops_waiting_and_tells_the_model_the_question_was_cancelled() {
        let ask = chat_tool_call_events("call-1", "ask_user_question", QUESTION_ARGUMENTS);
        let server = FakeServer::start([
            Reply::sse(&ask),
            Reply::sse(&chat_text_events(&["Asking in text instead."])),
        ]);
        let mut harness = Harness::start(&server).await;
        harness.submit("pick for me");
        let (turn_id, request) = asked(&mut harness).await;
        harness.send(UiCommand::Cancel { turn_id });
        harness.send(UiCommand::QuestionAnswered {
            request_id: request.id,
            answers: None,
        });
        let events = harness.until(finished(TurnOutcome::Interrupted)).await;
        assert!(events.iter().any(|event| matches!(
            event,
            UiEvent::ToolFinished { content, .. } if content == "(user cancelled the question)"
        )));
        harness.send(UiCommand::QuestionAnswered {
            request_id: request.id,
            answers: Some(vec!["Yes".to_owned()]),
        });
        harness.submit("go on");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(
            tool_results(&server.requests()[1].json()),
            ["(user cancelled the question)"]
        );
    }

    const MCP_SERVER: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"docs\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*)
      reply "$id" '{"tools":[{"name":"search","inputSchema":{"type":"object"}}]}' ;;
  esac
done
"#;

    fn mcp_notice(event: &UiEvent) -> bool {
        matches!(event, UiEvent::Notice { notice } if notice.topic == "mcp")
    }

    async fn mcp_notices(harness: &mut Harness, count: usize) -> Vec<String> {
        let mut notices = Vec::new();
        while notices.len() < count {
            let shown = harness.until(mcp_notice).await;
            notices.extend(notice_body(shown));
        }
        notices
    }

    #[tokio::test]
    async fn mcp_commands_that_need_a_home_say_it_is_unavailable() {
        let server = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let setup = agent_setup(&home, &server).await.without_preferences();
        let mut harness = Harness::with_setup(home, setup);
        for command in ["/mcp path", "/mcp reload", "/mcp trust approve docs"] {
            harness.command(command);
            assert_eq!(
                mcp_notices(&mut harness, 1).await,
                ["mcp|HOME is not available."],
                "{command}"
            );
        }
        harness.command("/mcp");
        assert_eq!(
            mcp_notices(&mut harness, 1).await,
            ["mcp|MCP: no servers configured. Use /mcp add <name> <command> [args...]."]
        );
        harness.command("/mcp resource list");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(
            notice_body(shown),
            ["|usage: /mcp resource list <server> or /mcp resource templates <server>"]
        );
    }

    #[tokio::test]
    async fn mcp_reload_reports_that_it_started_and_how_it_finished() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        harness.command("/mcp reload");
        assert_eq!(
            mcp_notices(&mut harness, 2).await,
            [
                "mcp|MCP reconnection started. Your existing MCP servers will stay active while the new configuration is checked.",
                "mcp|MCP configuration reloaded. No servers are configured."
            ]
        );
    }

    #[tokio::test]
    async fn trusting_a_project_server_saves_the_choice_and_starts_it() {
        let server = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let workspace = fs::canonicalize(home.path()).unwrap().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            workspace.join(".mcp.json"),
            json!({"mcpServers": {"docs": {"command": "/bin/sh", "args": ["-c", MCP_SERVER]}}})
                .to_string(),
        )
        .unwrap();
        let setup = agent_setup(&home, &server).await;
        let mut harness = Harness::with_setup(home, setup);
        harness.command("/mcp");
        assert_eq!(
            mcp_notices(&mut harness, 1).await,
            [
                "mcp|MCP: 1 server — 0 ready, 0 connecting, 0 needs auth, 0 failed. Pending approval: docs. Use /mcp list for details."
            ]
        );
        harness.command("/mcp trust approve docs");
        assert_eq!(
            mcp_notices(&mut harness, 2).await,
            [
                "mcp|Approving project MCP server 'docs'.",
                "mcp|MCP configuration reloaded successfully."
            ]
        );
        let settings: Value = serde_json::from_str(
            &fs::read_to_string(harness.home.path().join("config/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings["workspaces"][workspace.to_string_lossy().as_ref()]["enabledMcpjsonServers"],
            json!(["docs"])
        );
        harness.command("/mcp list");
        let listing = mcp_notices(&mut harness, 1).await.pop().unwrap();
        assert!(
            listing.contains(
                "docs source=workspace scope=workspace policy=optional transport=stdio state=ready"
            ),
            "{listing}"
        );
        assert!(listing.contains("    admission=approved\n"), "{listing}");
        harness.command("/mcp trust reject docs");
        let notices = mcp_notices(&mut harness, 2).await;
        assert_eq!(notices[0], "mcp|Rejecting project MCP server 'docs'.");
        assert_eq!(notices[1], "mcp|MCP configuration reloaded successfully.");
        harness.command("/mcp list");
        let listing = mcp_notices(&mut harness, 1).await.pop().unwrap();
        assert!(listing.contains("state=disabled"), "{listing}");
        assert!(listing.contains("    admission=rejected\n"), "{listing}");
    }

    async fn approved_docs_harness() -> (Harness, std::path::PathBuf) {
        let server = FakeServer::start([]);
        let home = tempfile::tempdir().unwrap();
        let workspace = fs::canonicalize(home.path()).unwrap().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            workspace.join(".mcp.json"),
            json!({"mcpServers": {"docs": {"command": "/bin/sh", "args": ["-c", MCP_SERVER]}}})
                .to_string(),
        )
        .unwrap();
        let setup = agent_setup(&home, &server).await;
        let mut harness = Harness::with_setup(home, setup);
        harness.command("/mcp trust approve docs");
        assert_eq!(
            mcp_notices(&mut harness, 2).await[1],
            "mcp|MCP configuration reloaded successfully."
        );
        (harness, workspace)
    }

    fn save_workspace_choices(harness: &Harness, workspace: &std::path::Path, choices: Value) {
        let path = harness.home.path().join("config/settings.json");
        let mut settings: Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        settings["workspaces"][workspace.to_string_lossy().as_ref()] = choices;
        fs::write(&path, settings.to_string()).unwrap();
    }

    async fn docs_listing(harness: &mut Harness) -> String {
        harness.command("/mcp list");
        mcp_notices(harness, 1).await.pop().unwrap()
    }

    #[tokio::test]
    async fn rejecting_a_server_settings_already_reject_still_retires_it() {
        let (mut harness, workspace) = approved_docs_harness().await;
        save_workspace_choices(
            &harness,
            &workspace,
            json!({"disabledMcpjsonServers": ["docs"]}),
        );
        assert!(docs_listing(&mut harness).await.contains("state=ready"));
        harness.command("/mcp trust reject docs");
        let notices = mcp_notices(&mut harness, 2).await;
        assert_eq!(notices[0], "mcp|Rejecting project MCP server 'docs'.");
        let listing = docs_listing(&mut harness).await;
        assert!(listing.contains("state=disabled"), "{listing}");
        assert!(listing.contains("    admission=rejected\n"), "{listing}");
    }

    #[tokio::test]
    async fn resetting_choices_settings_already_cleared_still_retires_approved_servers() {
        let (mut harness, workspace) = approved_docs_harness().await;
        save_workspace_choices(&harness, &workspace, json!({}));
        harness.command("/mcp trust reset");
        mcp_notices(&mut harness, 2).await;
        let listing = docs_listing(&mut harness).await;
        assert!(!listing.contains("state=ready"), "{listing}");
        assert!(!listing.contains("admission=approved"), "{listing}");
    }
}
