use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_agent::{Agent, Compaction, CompactionError, QuestionRequests, TurnFailure, TurnReport};
use ofx_config::save_model_preference;
use ofx_contract::{
    CompactionActivity, CompactionEnd, Notice, NoticeTone, ProviderError, QuestionRequest,
    ResumeRefusal, SessionCursor, SessionScope, SkillBinding, TurnId, TurnOutcome, UiCommand,
    UiEvent,
};
use ofx_session::SessionError;
use ofx_tui::Clipboard;
use ofx_workspace::ChangeTracker;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::app_bootstrap_runtime::{AgentSetup, CredentialSource};
use crate::app_commands::{
    CommandEffect, Work, handle_command, refuse_resume_during_turn, toggle_fast,
};
use crate::app_mcp_runtime::McpHost;
use crate::app_permission_runtime::PermissionRuntime;
use crate::app_session_runtime::{Persistence, RestoredPreferences};
use crate::native::NativeClipboard;
use crate::session_commands::SettingsAccess;
use crate::skills::HostSkills;
use crate::user_settings::{self, unsaved_notice};

pub(crate) type Emit = Arc<dyn Fn(UiEvent) + Send + Sync>;

const CONTEXT_TOPIC: &str = "context";
const MODEL_TOPIC: &str = "model";
const LEGACY_CONTEXT_LINE: &str = "[context]";
const LEGACY_CONTEXT_PREFIX: &str = "[context] ";

pub(crate) struct ControllerState {
    setup: AgentSetup,
    model: String,
    fast_mode: bool,
    config_pending: bool,
    pending_clear: Option<u64>,
    received_prompts: u64,
    queue: VecDeque<Prompt>,
    permissions: PermissionRuntime,
    context_notices: Arc<Mutex<ContextNotices>>,
    emit: Emit,
    clipboard: Arc<dyn Clipboard>,
    last_reply: Option<Arc<str>>,
    history_turns: usize,
    context_to_compact: bool,
    mcp: Option<McpHost>,
}

struct Prompt {
    text: String,
    skills: Vec<SkillBinding>,
}

impl ControllerState {
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn models(&self) -> &[String] {
        self.setup.models()
    }

    pub(crate) fn permissions(&self) -> &PermissionRuntime {
        &self.permissions
    }

    pub(crate) fn fast_mode(&self) -> bool {
        self.fast_mode
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        self.fast_mode = enabled;
    }

    pub(crate) fn save_model_preference(&self, topic: &str) {
        let provider = self.setup.provider();
        let saved = user_settings::save(self.setup.preferences(), |paths| {
            save_model_preference(paths, &provider, &self.model, self.fast_mode)
        });
        if let Err(unsaved) = saved {
            self.emit(UiEvent::Notice {
                notice: unsaved_notice(topic, &unsaved),
            });
        }
    }

    pub(crate) async fn supports_fast_mode(&self) -> bool {
        self.setup.supports_fast_mode(&self.model).await
    }

    pub(crate) fn status_body(&self) -> String {
        self.setup
            .status(&self.model, self.history_turns)
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

    pub(crate) fn change_tracker(&self) -> Option<&ChangeTracker> {
        self.setup.change_tracker()
    }

    pub(crate) fn clipboard(&self) -> &dyn Clipboard {
        &*self.clipboard
    }

    pub(crate) fn skills(&self) -> &HostSkills {
        self.setup.skills()
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

    fn select_model(&mut self, model: String) {
        if model != self.model {
            self.fast_mode = false;
        }
        self.use_model(model);
        self.save_model_preference(MODEL_TOPIC);
    }

    fn use_model(&mut self, model: String) {
        self.model = model;
        self.emit(UiEvent::ModelSelected {
            model: self.model.clone(),
        });
    }

    fn receive_prompt(&mut self, text: String, skills: Vec<SkillBinding>) {
        self.received_prompts += 1;
        self.queue.push_back(Prompt { text, skills });
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
}

impl Controller {
    pub(crate) fn new(
        mut setup: AgentSetup,
        emit: Emit,
        persistence: Option<Persistence>,
        pick_at_start: bool,
    ) -> Self {
        let questions = setup.take_question_requests();
        let notices = ContextNotices {
            startup: setup.context_notices().to_vec(),
            claimed: HashSet::new(),
        };
        let mcp = setup.mcp_host(Arc::clone(&emit));
        let state = ControllerState {
            model: setup.model().to_owned(),
            permissions: setup.permission_runtime(Arc::clone(&emit)),
            fast_mode: setup.fast_mode(),
            setup,
            config_pending: false,
            pending_clear: None,
            received_prompts: 0,
            queue: VecDeque::new(),
            context_notices: Arc::new(Mutex::new(notices)),
            emit,
            clipboard: Arc::new(NativeClipboard),
            last_reply: None,
            history_turns: 0,
            context_to_compact: false,
            mcp,
        };
        Self {
            agent: state.setup.agent(),
            state,
            persistence,
            questions,
            pick_at_start,
        }
    }

    #[cfg(test)]
    fn with_clipboard(mut self, clipboard: Arc<dyn Clipboard>) -> Self {
        self.state.clipboard = clipboard;
        self
    }

    pub(crate) async fn run(mut self, mut commands: UnboundedReceiver<UiCommand>) {
        self.show_startup_notices();
        if !self.pick_at_start {
            let opened = self
                .persistence
                .as_mut()
                .and_then(|persistence| persistence.open(&mut self.agent));
            self.session_notice(opened);
        }
        self.remember_agent_facts();
        self.serve(&mut commands).await;
        if let Some(persistence) = &mut self.persistence {
            persistence.close(&mut self.agent);
        }
    }

    async fn serve(&mut self, commands: &mut UnboundedReceiver<UiCommand>) {
        loop {
            if let Some(prompt) = self.state.queue.pop_front() {
                if !self.run_turn(&prompt, commands).await {
                    return;
                }
                continue;
            }
            let Some(command) = commands.recv().await else {
                return;
            };
            match command {
                UiCommand::Submit { prompt, skills } => self.state.receive_prompt(prompt, skills),
                UiCommand::RunCommand { text } => {
                    if !self.run_idle_command(&text, commands).await {
                        return;
                    }
                }
                UiCommand::TogglePermissionMode => self.state.permissions.toggle_mode(),
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
        match handle_command(&self.state, text, Work::Idle) {
            CommandEffect::None => {}
            CommandEffect::SwitchModel(model) => {
                self.state.select_model(model);
                self.save_preferences();
                self.reconfigure();
            }
            CommandEffect::Clear => self.clear(self.state.received_prompts),
            CommandEffect::ToggleFast => {
                if toggle_fast(&mut self.state).await {
                    self.save_preferences();
                }
                self.reconfigure();
            }
            CommandEffect::Compact => return self.compact(commands).await,
            CommandEffect::OpenSessions => self.open_picker(SessionScope::CurrentWorkspace),
        }
        true
    }

    async fn compact(&mut self, commands: &mut UnboundedReceiver<UiCommand>) -> bool {
        self.state.compaction(CompactionActivity::Preparing);
        let cancel = CancellationToken::new();
        let emit = Arc::clone(&self.state.emit);
        let state = &mut self.state;
        let persistence = &mut self.persistence;
        let mut open = true;
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
                    command = commands.recv(), if open => match command {
                        None => {
                            open = false;
                            cancel.cancel();
                        }
                        Some(UiCommand::Submit { prompt, skills }) => {
                            state.receive_prompt(prompt, skills);
                        }
                        Some(UiCommand::CancelCompaction) => cancel.cancel(),
                        Some(UiCommand::TogglePermissionMode) => state.permissions.toggle_mode(),
                        Some(UiCommand::FullAccessWarningShown) => {
                            state.permissions.full_access_warning_shown();
                        }
                        Some(
                            UiCommand::Cancel { .. }
                            | UiCommand::Approval { .. }
                            | UiCommand::QuestionAnswered { .. },
                        ) => {}
                        Some(
                            command @ (UiCommand::OpenSessions { .. }
                            | UiCommand::ListSessions { .. }
                            | UiCommand::ResumeSession { .. }
                            | UiCommand::CloseSessionPicker),
                        ) => refuse_session_command(state, command),
                        Some(UiCommand::RunCommand { text }) => {
                            run_deferred_command(
                                state,
                                persistence,
                                &text,
                                Work::Compaction,
                                &cancel,
                            )
                            .await;
                        }
                    },
                }
            }
        };
        self.state.compaction(compaction_activity(result));
        self.settle_deferred_commands();
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
        match persistence.resume_selected(id, &mut self.agent) {
            Ok(switched) => {
                self.forget_tracked_changes();
                self.restore_preferences(switched.preferences);
                self.remember_agent_facts();
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
        self.state.fast_mode = restored.fast_mode;
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
        self.session_notice(started);
    }

    fn reconfigure(&mut self) {
        let mut config = self.state.setup.config(&self.state.model);
        config.fast_mode = self.state.fast_mode;
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

    fn save_preferences(&mut self) {
        let saved = save_session_preferences(&self.state, &mut self.persistence);
        self.session_notice(saved);
    }

    fn clear(&mut self, first_kept_prompt: u64) {
        self.agent.clear_history();
        self.forget_tracked_changes();
        let started = self
            .persistence
            .as_mut()
            .and_then(|persistence| persistence.begin_fresh(&mut self.agent));
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
        prompt: &Prompt,
        commands: &mut UnboundedReceiver<UiCommand>,
    ) -> bool {
        self.state.skills().refresh();
        let cancel = CancellationToken::new();
        let emit = Arc::clone(&self.state.emit);
        let running = Arc::new(Mutex::new(None));
        let started = Arc::clone(&running);
        let running_turn = || *running.lock().unwrap_or_else(PoisonError::into_inner);
        let notices = Arc::clone(&self.state.context_notices);
        let state = &mut self.state;
        let persistence = &mut self.persistence;
        let questions = &mut self.questions;
        let mut open = true;
        let report = {
            let mut sink = move |event: UiEvent| match event {
                UiEvent::TurnFinished { .. } => {}
                UiEvent::TurnStarted { turn_id } => {
                    *started.lock().unwrap_or_else(PoisonError::into_inner) = Some(turn_id);
                    emit(event);
                }
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
            };
            let turn =
                self.agent
                    .run_turn_with_skills(&prompt.text, &prompt.skills, &mut sink, &cancel);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    report = &mut turn => break report,
                    command = commands.recv(), if open => match command {
                        None => {
                            open = false;
                            cancel.cancel();
                        }
                        Some(UiCommand::Submit { prompt, skills }) => {
                            state.receive_prompt(prompt, skills);
                        }
                        Some(UiCommand::Cancel { turn_id }) => {
                            if running_turn() == Some(turn_id) {
                                cancel.cancel();
                            }
                        }
                        Some(UiCommand::Approval { request_id, decision }) => {
                            if let Some(approvals) = state.setup.approvals() {
                                approvals.resolve(request_id, decision);
                            }
                        }
                        Some(UiCommand::QuestionAnswered { request_id, answers }) => {
                            if let Some(questions) = state.setup.questions() {
                                questions.resolve(request_id, answers);
                            }
                        }
                        Some(UiCommand::TogglePermissionMode) => state.permissions.toggle_mode(),
                        Some(UiCommand::FullAccessWarningShown) => {
                            state.permissions.full_access_warning_shown();
                        }
                        Some(UiCommand::RunCommand { text }) => {
                            run_deferred_command(state, persistence, &text, Work::Turn, &cancel)
                                .await;
                        }
                        Some(
                            command @ (UiCommand::OpenSessions { .. }
                            | UiCommand::ListSessions { .. }
                            | UiCommand::ResumeSession { .. }
                            | UiCommand::CloseSessionPicker),
                        ) => refuse_session_command(state, command),
                        Some(UiCommand::CancelCompaction) => {}
                    },
                    request = next_question(questions) => relay_question(state, running_turn(), request),
                }
            }
        };
        if let Some(turn_id) = running_turn() {
            self.announce_turn_end(turn_id, &report);
        }
        self.finish_turn(&report);
        self.settle_deferred_commands();
        open
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

    fn settle_deferred_commands(&mut self) {
        self.remember_agent_facts();
        if std::mem::take(&mut self.state.config_pending) {
            self.reconfigure();
        }
        if let Some(first_kept_prompt) = self.state.pending_clear.take() {
            self.clear(first_kept_prompt);
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

async fn run_deferred_command(
    state: &mut ControllerState,
    persistence: &mut Option<Persistence>,
    text: &str,
    work: Work,
    cancel: &CancellationToken,
) {
    match handle_command(state, text, work) {
        CommandEffect::None | CommandEffect::Compact | CommandEffect::OpenSessions => return,
        CommandEffect::SwitchModel(model) => {
            state.config_pending = true;
            state.select_model(model);
        }
        CommandEffect::Clear => {
            state.pending_clear = Some(state.received_prompts);
            state.queue.clear();
            cancel.cancel();
            return;
        }
        CommandEffect::ToggleFast => {
            state.config_pending = true;
            if !toggle_fast(state).await {
                return;
            }
        }
    }
    if let Some(notice) = save_session_preferences(state, persistence) {
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

fn save_session_preferences(
    state: &ControllerState,
    persistence: &mut Option<Persistence>,
) -> Option<Notice> {
    persistence
        .as_mut()
        .and_then(|persistence| persistence.select_model(&state.model, state.fast_mode))
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
        TurnFailure::StepLimitReached | TurnFailure::RepeatedMalformedArguments => None,
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

    use ofx_config::{ProfilePaths, Settings};
    use ofx_contract::{
        ApprovalDecision, PermissionMode, ProviderErrorKind, SkillMenuFocus, ToolResultStatus,
        TurnId, TurnOutcome,
    };
    use ofx_exec::{ManagedExecutions, SessionSupervisor};
    use ofx_gateway::{CodexEndpoints, CodexModelsEndpoints};
    use ofx_testkit::{FakeServer, Reply, chat_text_events, chat_tool_call_events};
    use serde_json::{Value, json};
    use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
    use tokio::time::timeout;

    use super::*;
    use crate::app_bootstrap_runtime::{Launch, Profile};
    use crate::codex_provider::SubscriptionEndpoints;

    struct Harness {
        home: tempfile::TempDir,
        commands: UnboundedSender<UiCommand>,
        events: UnboundedReceiver<UiEvent>,
        seen: Vec<UiEvent>,
        clipboard: Arc<TestClipboard>,
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

    async fn agent_setup(home: &tempfile::TempDir, server: &FakeServer) -> AgentSetup {
        let settings = json!({
            "provider": "local",
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": server.base_url(),
                    "auth": {"type": "none"},
                    "models": ["model-a", "vendor/model-b"]
                }
            }
        });
        agent_setup_with(home, &settings, SubscriptionEndpoints::default()).await
    }

    async fn agent_setup_with(
        home: &tempfile::TempDir,
        settings: &Value,
        endpoints: SubscriptionEndpoints,
    ) -> AgentSetup {
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
        Profile::new(workspace, Some(home.path().into()), Some(paths), settings)
            .unwrap()
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
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    const CODEX_MODEL: &str = "gpt-6.1-sol";
    const OTHER_CODEX_MODEL: &str = "gpt-6.1-luna";

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

    impl Harness {
        async fn start(server: &FakeServer) -> Self {
            let home = tempfile::tempdir().unwrap();
            let setup = agent_setup(&home, server).await;
            Self::with_setup(home, setup)
        }

        async fn codex(codex: &FakeServer, catalog: &FakeServer) -> Self {
            let home = codex_home();
            let settings = json!({"provider": "codex", "models": {"codex": CODEX_MODEL}});
            let setup = agent_setup_with(&home, &settings, codex_endpoints(codex, catalog)).await;
            Self::with_setup(home, setup)
        }

        fn with_setup(home: tempfile::TempDir, setup: AgentSetup) -> Self {
            let (events_sender, events) = unbounded_channel();
            let emit: Emit = Arc::new(move |event| {
                let _ = events_sender.send(event);
            });
            let (commands, receiver) = unbounded_channel();
            let clipboard = Arc::new(TestClipboard::default());
            let shared: Arc<dyn Clipboard> = clipboard.clone();
            tokio::spawn(
                Controller::new(setup, emit, None, false)
                    .with_clipboard(shared)
                    .run(receiver),
            );
            Self {
                home,
                commands,
                events,
                seen: Vec::new(),
                clipboard,
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
    async fn prompts_submitted_during_a_turn_run_next_in_order() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["one"])),
            Reply::sse(&chat_text_events(&["two"])),
        ]);
        let mut harness = Harness::start(&server).await;
        harness.submit("first");
        harness.submit("second");
        harness.until(finished(TurnOutcome::Completed)).await;
        let second = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(matches!(second[0], UiEvent::TurnStarted { .. }));
        assert!(
            second
                .iter()
                .any(|event| matches!(event, UiEvent::AssistantText { text, .. } if text == "two"))
        );
        let requests = server.requests();
        assert_eq!(user_messages(&requests[1].json()), 2);
    }

    #[tokio::test]
    async fn model_commands_show_and_switch_the_connection_models() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["ok"]))]);
        let mut harness = Harness::start(&server).await;
        harness.command("/model");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(
            notice_body(shown),
            ["model|model-a\navailable: model-a, vendor/model-b"]
        );
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
    async fn reset_during_a_turn_cancels_it_and_drops_the_prompts_queued_before_it() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([held, Reply::sse(&chat_text_events(&["after"]))]);
        let mut harness = Harness::start(&server).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        harness.submit("dropped");
        harness.command("/reset");
        harness.submit("kept");
        harness.until(finished(TurnOutcome::Interrupted)).await;
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        let next = timeout(
            Duration::from_secs(10),
            harness.until(finished(TurnOutcome::Completed)),
        )
        .await
        .expect("the prompt sent after reset runs");
        assert!(
            next.iter().any(
                |event| matches!(event, UiEvent::AssistantText { text, .. } if text == "after")
            )
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(user_messages(&requests[1].json()), 1);
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
                "status|model=model-a\nmodel_source=local\nprovider_endpoint={}\nauth=configured provider\nconnected_providers=local\nauth_refreshable=false\npermission_mode=auto\nworkspace={}\nhistory_turns={turns}\nsession_permission_grants={grants}\nagent_step_limit=0",
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
        assert_eq!(user_messages(&body), 6);
        assert_eq!(first_user_message(&body), "read the notes");
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
        assert_eq!(user_messages(&server.requests()[7].json()), 6);
    }

    async fn fast_notice(harness: &mut Harness) -> String {
        harness.command("/fast");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { notice } if notice.topic == "fast"))
            .await;
        notice_body(shown).pop().unwrap()
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
        let held = Reply::held_sse(&[json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","phase":"final_answer"}}).to_string(), json!({"type":"response.output_text.delta","output_index":0,"delta":"partial\n"}).to_string()]);
        let codex = FakeServer::start([held, codex_text("next")]);
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
    async fn prompts_submitted_after_a_mid_turn_clear_run_in_the_fresh_conversation() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([held, Reply::sse(&chat_text_events(&["after"]))]);
        let mut harness = Harness::start(&server).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        harness.submit("dropped");
        harness.command("/clear");
        harness.submit("kept");
        let cleared = harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        assert_eq!(
            cleared.last(),
            Some(&UiEvent::ConversationCleared {
                first_kept_prompt: 2
            })
        );
        let next = timeout(
            Duration::from_secs(10),
            harness.until(finished(TurnOutcome::Completed)),
        )
        .await
        .expect("the prompt sent after clear runs");
        assert!(
            next.iter().any(
                |event| matches!(event, UiEvent::AssistantText { text, .. } if text == "after")
            )
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(user_messages(&requests[1].json()), 1);
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
            ["skills|Skill installation is not available yet."]
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
}
