mod settings_menu;

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_agent::{
    Agent, Compaction, CompactionError, QuestionRequests, QueuedPrompt, TurnFailure, TurnReport,
    WorkerRuntime,
};
use ofx_config::save_model_preference;
use ofx_contract::{
    BoxFuture, CompactionActivity, CompactionEnd, ModelCatalog, ModelOption, Notice, NoticeTone,
    ProviderError, QuestionRequest, ReasoningEffort, ResumeRefusal, SessionCursor, SessionScope,
    SkillBinding, StatuslineItem, StatuslineToggles, TurnId, TurnOutcome, UiCommand, UiEvent,
};
use ofx_session::{SessionError, prompt_display_title};
use ofx_tui::Clipboard;
use ofx_workspace::ChangeTracker;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::app_bootstrap_runtime::{AgentSetup, CredentialSource};
use crate::app_commands::{
    CommandEffect, ModelChange, ModelPick, Outcome, Work, change_model, handle_command, listed,
    refuse_resume_during_turn, rename_session,
};
use crate::app_mcp_runtime::McpHost;
use crate::app_permission_runtime::PermissionRuntime;
use crate::app_session_runtime::{Persistence, RestoredPreferences, SessionTitle};
use crate::approval_queue::ApprovalQueue;
use crate::model_cache_runtime::ModelSource;
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

    pub(crate) fn effort(&self) -> &ReasoningEffort {
        &self.effort
    }

    pub(crate) fn fast_mode(&self) -> bool {
        self.speed == Speed::Fast
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
        let prompt = QueuedPrompt::new(self.received_prompts, text, skills);
        self.received_prompts += 1;
        if installing {
            self.pending_install_inputs
                .push_back(InstallInput::Prompt(prompt));
        } else {
            self.worker.admit(prompt);
        }
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
    pick_at_start: bool,
    catalog: CatalogFetch,
    herdr: Option<Arc<crate::herdr::Herdr>>,
    installation: Option<InstallTask>,
}

struct CatalogFetch {
    source: ModelSource,
    provider: String,
    pending: Option<BoxFuture<'static, ModelCatalog>>,
    waiting: Vec<ModelChange>,
    settings: Option<SettingsUpdate>,
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

    async fn next_command(
        &mut self,
        commands: &mut UnboundedReceiver<UiCommand>,
        state: &mut ControllerState,
        persistence: &mut Option<Persistence>,
        work: Work,
    ) -> Option<UiCommand> {
        loop {
            let Some(fetch) = &mut self.pending else {
                return commands.recv().await;
            };
            tokio::select! {
                command = commands.recv() => return command,
                catalog = fetch => self.arrived(state, persistence, catalog, work),
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
            },
            state,
            persistence,
            questions,
            pick_at_start,
            herdr: None,
            installation: None,
        }
    }

    pub(crate) fn with_herdr(mut self, herdr: Option<Arc<crate::herdr::Herdr>>) -> Self {
        self.herdr = herdr;
        self
    }

    pub(crate) fn requesting_ultrafast(mut self, requested: bool) -> Self {
        if requested {
            self.state.speed = Speed::UltraRequested;
        }
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
            let resumed_title = self
                .persistence
                .as_ref()
                .and_then(Persistence::resumed_title);
            let opened = self
                .persistence
                .as_mut()
                .and_then(|persistence| persistence.open(&mut self.agent));
            if let Some(title) = resumed_title {
                self.state.session_title.set(Some(&title));
            }
            self.bind_children();
            self.session_notice(opened);
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

    async fn serve(&mut self, commands: &mut UnboundedReceiver<UiCommand>) {
        loop {
            if self.installation.is_none()
                && let Some(prompt) = self.state.worker.take_next()
            {
                if !self.run_turn(&prompt, commands).await {
                    return;
                }
                continue;
            }
            let next = self.catalog.next_command(
                commands,
                &mut self.state,
                &mut self.persistence,
                Work::Idle,
            );
            let command = tokio::select! {
                result = wait_install(&mut self.installation) => {
                    complete_install(&self.state, &mut self.installation, result);
                    self.settle_deferred_commands(true).await;
                    continue;
                }
                command = next => command,
            };
            let Some(command) = command else {
                return;
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
                UiCommand::ListSessions {
                    scope,
                    after,
                    limit,
                } => self.list_sessions(scope, after, limit),
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
            CommandEffect::Compact => return self.compact(commands).await,
            CommandEffect::OpenSessions => self.open_picker(SessionScope::CurrentWorkspace),
            CommandEffect::OpenSettings => self.open_settings_menu().await,
            CommandEffect::Rename(title) => {
                rename_session(&self.state, self.persistence.as_mut(), &title);
            }
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
                        Some(command) => run_deferred(state, persistence, catalog, command, installation, work, &cancel),
                    },
                }
            }
        };
        self.state.worker.finish_processing();
        self.state.compaction(compaction_activity(result));
        self.settle_deferred_commands(open).await;
        open
    }

    fn open_picker(&self, scope: SessionScope) {
        self.state.emit(UiEvent::SessionPickerOpened { scope });
    }

    fn list_sessions(&mut self, scope: SessionScope, after: Option<SessionCursor>, limit: usize) {
        let more = after.is_some();
        let listed = self
            .persistence
            .as_mut()
            .map_or(Err(SessionError::SessionStoreUnavailable), |persistence| {
                persistence.page(scope, after, limit)
            });
        match listed {
            Ok(page) => self.state.emit(UiEvent::SessionsListed { page }),
            Err(error) => {
                let action = if more {
                    "unable to load more saved sessions"
                } else {
                    "unable to list saved sessions"
                };
                self.state
                    .notice(NoticeTone::Error, "session", &format!("{action}: {error}"));
                self.state.emit(UiEvent::SessionsUnavailable { scope });
            }
        }
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
                self.session_notice(switched.notice);
            }
            Err(refused) => {
                self.session_notice(refused.notice);
                self.refuse_resume(id, refused.refusal);
            }
        }
    }

    fn restore_preferences(&mut self, restored: RestoredPreferences) {
        self.state
            .setup
            .restore_reasoning(restored.reasoning_effort, restored.fast_mode);
        self.state.effort = self.state.setup.reasoning_effort();
        self.state.set_fast_mode(restored.fast_mode);
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
        let mut sink = turn_events(
            Arc::clone(&self.state.emit),
            Arc::clone(&running),
            self.state.setup.approvals().cloned(),
            Arc::clone(&self.state.context_notices),
        );
        let state = &mut self.state;
        let persistence = &mut self.persistence;
        let questions = &mut self.questions;
        let catalog = &mut self.catalog;
        let work = Work::Turn;
        let mut open = true;
        let installation = &mut self.installation;
        let report = {
            let turn =
                self.agent
                    .run_turn_with_skills(&prompt.text, &prompt.skills, &mut sink, &cancel);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    report = &mut turn => break report,
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
                        Some(UiCommand::Approval { request_id, decision }) => {
                            state.setup.answer_approval(request_id, decision);
                        }
                        Some(UiCommand::QuestionAnswered { request_id, answers }) => {
                            if let Some(questions) = state.setup.questions() {
                                questions.resolve(request_id, answers);
                            }
                        }
                        Some(command) => run_deferred(state, persistence, catalog, command, installation, work, &cancel),
                    },
                    request = next_question(questions) => relay_question(state, running_turn(), request),
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
            CommandEffect::OpenSettings => return catalog.open_settings_menu(state),
            CommandEffect::Rename(title) => {
                return rename_session(state, persistence.as_mut(), &title);
            }
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
        | UiCommand::Cancel { .. }
        | UiCommand::PauseRecovery { .. }
        | UiCommand::Approval { .. }
        | UiCommand::QuestionAnswered { .. }
        | UiCommand::CancelCompaction => return,
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
    let Outcome::Changed { effort } = change_model(state, change, models, work) else {
        return;
    };
    state.config_pending = true;
    if let Some(notice) = save_session_preferences(state, persistence, effort.as_ref()) {
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

fn observe_prompt(persistence: Option<&Persistence>, prompt: &str) {
    if let Some(persistence) = persistence {
        persistence.observe_prompt(prompt);
    }
}

fn save_session_preferences(
    state: &ControllerState,
    persistence: &mut Option<Persistence>,
    effort: Option<&ReasoningEffort>,
) -> Option<Notice> {
    persistence
        .as_mut()
        .and_then(|persistence| persistence.select_model(&state.model, effort, state.fast_mode()))
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
    use ofx_session::{SessionPreferences, SessionStore};
    use ofx_testkit::{
        FakeServer, Gate, RecordedRequest, Reply, chat_text_events, chat_tool_call_events,
    };
    use serde_json::{Value, json};
    use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
    use tokio::time::timeout;

    use super::*;
    use crate::app_bootstrap_runtime::{Launch, Profile};
    use crate::app_session_runtime::{LaunchOverrides, session_route};
    use crate::codex_provider::SubscriptionEndpoints;

    mod steering;

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
            let home = codex_home();
            let setup = agent_setup_with(&home, settings, codex_endpoints(codex, catalog)).await;
            Self::saved(home, setup)
        }

        async fn start_saved(server: &FakeServer) -> Self {
            let home = tempfile::tempdir().unwrap();
            let setup = agent_setup(&home, server).await;
            Self::saved(home, setup)
        }

        fn saved(home: tempfile::TempDir, setup: AgentSetup) -> Self {
            let workspace = fs::canonicalize(home.path().join("workspace")).unwrap();
            let store =
                SessionStore::open(&home.path().join("data"), workspace.to_str().unwrap()).unwrap();
            let route = session_route(&setup).unwrap();
            let preferences = SessionPreferences {
                provider: route.provider.clone(),
                model: setup.configured_model().to_owned(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
            };
            let overrides = LaunchOverrides {
                model: None,
                effort: None,
                fast_mode: None,
            };
            let persistence = Persistence::new(store, route, preferences, overrides, None);
            Self::spawn(home, setup, Some(persistence))
        }

        fn with_setup(home: tempfile::TempDir, setup: AgentSetup) -> Self {
            Self::spawn(home, setup, None)
        }

        fn with_setup_observer(
            home: tempfile::TempDir,
            setup: AgentSetup,
            observe: impl Fn(&UiEvent) + Send + Sync + 'static,
        ) -> Self {
            Self::spawn_observer(home, setup, None, false, observe)
        }

        fn requesting_ultrafast(home: tempfile::TempDir, setup: AgentSetup) -> Self {
            Self::spawn_observer(home, setup, None, true, |_| {})
        }

        fn spawn(
            home: tempfile::TempDir,
            setup: AgentSetup,
            persistence: Option<Persistence>,
        ) -> Self {
            Self::spawn_observer(home, setup, persistence, false, |_| {})
        }

        fn spawn_observer(
            home: tempfile::TempDir,
            setup: AgentSetup,
            persistence: Option<Persistence>,
            ultrafast: bool,
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
            decision: ApprovalDecision::Always,
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
        assert_eq!(
            switched(&mut harness, "codex").await,
            [
                "provider|Preparing Codex subscription.",
                "provider|Run oh-fx login codex, then try switching again."
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
    async fn workspace_reports_that_workspace_access_is_unavailable_for_every_form() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        for command in [
            "/workspace",
            "/workspace list",
            "/workspace add ../other",
            "/workspace clear",
        ] {
            harness.command(command);
            let shown = harness
                .until(|event| matches!(event, UiEvent::Notice { .. }))
                .await;
            assert_eq!(
                notice_body(shown),
                ["workspace|Workspace access is unavailable in this runtime."],
                "{command}"
            );
            let Some(UiEvent::Notice { notice }) = shown.last() else {
                unreachable!()
            };
            assert_eq!(notice.tone, NoticeTone::Error);
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
                decision,
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
            decision: ApprovalDecision::Once,
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
            decision: ApprovalDecision::Always,
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
            decision: ApprovalDecision::Always,
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
            decision,
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
        harness.send(UiCommand::Approval {
            request_id: request.id,
            decision: ApprovalDecision::Always,
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
            decision: ApprovalDecision::Always,
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
            "⚠ Codex subscription authentication failed · HTTP 401 · Run oh-fx login codex to sign in again."
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
