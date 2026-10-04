use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use ofx_contract::UiEvent::{AssistantBoundary, ContextNotice, ToolProvisional};
use ofx_contract::{
    CallDescription, CompactionActivity, CompactionEnd, Notice, NoticeTone, RouteRecoveryStatus,
    ToolActivity, TurnId, TurnOutcome, UiCommand, UiEvent, Usage,
};
use ofx_markdown::{Completions, MarkdownProcessor};

use super::leading_whitespace::LeadingWhitespace;
use super::{ActiveTurn, FreshScreen, Shell, Submission, SubmissionState};
use crate::footer::file_approval::FileApproval;
use crate::output::activity_status::TurnPhase;
use crate::output::compaction_activity::CompactionStatus;
use crate::output::recovery_status::RecoveryStatus;
use crate::render_engine::transcript_blocks::Entry;
use crate::transcript::tool_presentation::{Finished, Rejected, ToolActivityRow};

const PARAGRAPH_BREAK: &str = "\n\n";
const ASK_USER_QUESTION: &str = "ask_user_question";
const SYSTEM_NOTICE_TOPIC: &str = "system";

#[derive(Clone)]
pub struct UiEventSender {
    events: Sender<Delivery>,
    wake: Arc<UnixStream>,
    woken: Arc<AtomicBool>,
}

impl UiEventSender {
    pub fn send(&self, event: UiEvent) {
        let delivery = Delivery::prepare(event);
        if self.events.send(delivery).is_ok() && !self.woken.swap(true, Ordering::AcqRel) {
            let _ = (&*self.wake).write(&[1]);
        }
    }
}

struct Delivery {
    event: UiEvent,
    file: Option<Box<FileApproval>>,
}

impl Delivery {
    fn prepare(mut event: UiEvent) -> Self {
        let file = match &mut event {
            UiEvent::ApprovalRequested { request, .. } => {
                let change = request.change.take();
                request
                    .file
                    .as_ref()
                    .map(|file| Box::new(FileApproval::new(request, file, change.as_ref())))
            }
            _ => None,
        };
        Self { event, file }
    }
}

pub struct UiEventReceiver {
    events: Receiver<Delivery>,
    wake: UnixStream,
    woken: Arc<AtomicBool>,
}

impl UiEventReceiver {
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.wake.as_fd()
    }

    fn drain_wake(&self) {
        let mut sink = [0_u8; 64];
        while matches!((&self.wake).read(&mut sink), Ok(count) if count > 0) {}
        self.woken.swap(false, Ordering::AcqRel);
    }
}

pub fn ui_channel() -> io::Result<(UiEventSender, UiEventReceiver)> {
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    let (events, receiver) = mpsc::channel();
    let woken = Arc::new(AtomicBool::new(false));
    Ok((
        UiEventSender {
            events,
            wake: Arc::new(writer),
            woken: Arc::clone(&woken),
        },
        UiEventReceiver {
            events: receiver,
            wake: reader,
            woken,
        },
    ))
}

#[derive(Clone, Copy)]
enum Streamed {
    Shown,
    Counted,
    Unchanged,
}

impl Streamed {
    fn counted(changed: bool) -> Self {
        if changed {
            Self::Counted
        } else {
            Self::Unchanged
        }
    }
}

impl Shell<'_> {
    pub(super) fn drain_ui_events(&mut self) {
        self.events.drain_wake();
        loop {
            match self.events.events.try_recv() {
                Ok(delivery) => self.handle_ui_event(delivery),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.should_exit = true;
                    return;
                }
            }
        }
    }

    fn handle_ui_event(&mut self, delivery: Delivery) {
        let Delivery { event, file } = delivery;
        match event {
            UiEvent::AssistantText { turn_id, text } => {
                if self.is_visible_turn(turn_id) {
                    let streamed = self.assistant_text(&text);
                    self.note_streamed(streamed);
                }
            }
            UiEvent::ReasoningText { turn_id, text } => {
                if let Some(turn) = self.visible_turn(turn_id) {
                    let before = turn.tokens.progress();
                    turn.tokens.consume_reasoning(&text);
                    let streamed = Streamed::counted(turn.tokens.progress() != before);
                    self.note_streamed(streamed);
                }
            }
            event => {
                self.mark_dirty();
                self.handle_presented_event(event, file);
            }
        }
    }

    fn note_streamed(&mut self, streamed: Streamed) {
        match streamed {
            Streamed::Counted if self.frame.activity_visible => self.frame.tokens_due = true,
            Streamed::Shown | Streamed::Counted => self.mark_dirty(),
            Streamed::Unchanged => {}
        }
    }

    fn handle_presented_event(&mut self, event: UiEvent, file: Option<Box<FileApproval>>) {
        match event {
            UiEvent::TurnStarted { turn_id } => self.turn_started(turn_id),
            event @ (UiEvent::AssistantRestarted { .. } | UiEvent::Operational { .. }) => {
                self.turn_text(event);
            }
            UiEvent::ApprovalRequested { turn_id, request } => {
                self.end_assistant_step(turn_id);
                self.approval_requested(turn_id, *request, file);
            }
            UiEvent::QuestionRequested { turn_id, request } => {
                self.end_assistant_step(turn_id);
                self.question_requested(turn_id, request);
            }
            event @ (UiEvent::ToolStarted { .. }
            | UiEvent::ToolRejected { .. }
            | UiEvent::ToolFinished { .. }
            | UiEvent::ToolDeferred { .. }
            | UiEvent::SubagentStatus { .. }
            | UiEvent::ApprovalFeedback { .. }) => self.tool_event(event),
            UiEvent::SteeringApplied {
                turn_id,
                prompt,
                text,
            } => self.steering_applied(turn_id, prompt, text),
            AssistantBoundary { .. }
            | ToolProvisional { .. }
            | ContextNotice { .. }
            | UiEvent::AssistantText { .. }
            | UiEvent::ReasoningText { .. } => {}
            UiEvent::Recovery { turn_id, status } => self.recovery_reported(turn_id, status),
            UiEvent::UsageReported {
                turn_id,
                usage,
                context_window,
            } => self.usage_reported(turn_id, usage, context_window),
            UiEvent::TurnFinished { turn_id, outcome } => self.turn_finished(turn_id, outcome),
            UiEvent::ApiStatus { turn_id, text } => {
                if let Some(turn) = self.visible_turn(turn_id) {
                    turn.failure = Some(text);
                }
            }
            UiEvent::Notice { notice } => self.push_entry(Entry::Notice(notice)),
            UiEvent::SystemNotice { text } => self.push_entry(Entry::Notice(Notice::new(
                NoticeTone::Neutral,
                SYSTEM_NOTICE_TOPIC,
                text,
            ))),
            UiEvent::ModelSelected { model } => self.model_selected(model),
            UiEvent::ModelControlsChanged { controls } => self.options.model_controls = controls,
            UiEvent::SessionTitleChanged { title } => self.session_title_changed(title),
            UiEvent::StatuslineChanged { item, enabled } => self.statusline.set(item, enabled),
            event @ (UiEvent::ModelCatalog { .. }
            | UiEvent::ProviderPicker { .. }
            | UiEvent::ProviderSelected { .. }) => self.provider_event(event),
            UiEvent::StatuslineMenuOpened => self.open_statusline_menu(),
            UiEvent::SettingsMenuOpened { snapshot } => self.open_settings_menu(snapshot),
            UiEvent::SettingsChanged { snapshot } => self.settings_changed(snapshot),
            UiEvent::PromptHistoryChanged { enabled } => self.prompt_history_changed(enabled),
            UiEvent::LoginChanged { missing } => self.options.login_missing = missing,
            UiEvent::PromptHeld => self.prompt_held(),
            UiEvent::HeldPromptDropped => self.held_prompt_dropped(),
            UiEvent::PermissionModeChanged {
                mode,
                full_access_warning,
            } => self.permission_mode_changed(mode, full_access_warning),
            UiEvent::HelpRequested => self.open_help_menu(),
            UiEvent::StatsRequested => self.stats_requested(),
            UiEvent::CompactionActivity { activity } => self.compaction_activity(activity),
            UiEvent::TurnCompaction { turn_id, activity } => {
                if self.is_visible_turn(turn_id) {
                    self.turn_compaction(turn_id, activity);
                }
            }
            UiEvent::UpgradeStatus { label } => self.upgrade_status_changed(label),
            UiEvent::SkillsMenu { items, focus } => self.open_skills_menu(items, &focus),
            UiEvent::ConversationCleared { first_kept_prompt } => {
                self.conversation_cleared(first_kept_prompt);
            }
            UiEvent::SessionPickerOpened { scope } => self.session_picker_opened(scope),
            UiEvent::SessionsListed { page } => self.sessions_listed(page),
            UiEvent::SessionsUnavailable { scope } => self.sessions_unavailable(scope),
            UiEvent::SessionResumeFailed { id, refusal } => {
                self.session_resume_failed(&id, refusal);
            }
            UiEvent::SessionResumed { history } => self.session_resumed(history),
            UiEvent::RecoveryContinuing { prompt, id } => self.recovery_continuing(prompt, id),
            UiEvent::ExitRequested => self.should_exit = true,
        }
    }

    fn usage_reported(&mut self, turn_id: TurnId, usage: Usage, context_window: Option<u32>) {
        self.statusline
            .usage_reported(usage.input_tokens, context_window);
        if let Some(turn) = self.visible_turn(turn_id) {
            turn.tokens.settle(usage.output_tokens);
        }
    }

    fn turn_text(&mut self, event: UiEvent) {
        match event {
            UiEvent::AssistantRestarted { turn_id, text } if self.is_visible_turn(turn_id) => {
                self.restart_assistant(&text);
            }
            UiEvent::Operational { turn_id, text } if self.is_visible_turn(turn_id) => {
                self.operational_text(&text);
            }
            _ => {}
        }
    }

    fn recovery_reported(&mut self, turn_id: TurnId, status: RouteRecoveryStatus) {
        let now_ms = self.now_ms();
        if let Some(turn) = self.visible_turn(turn_id) {
            turn.recovery = Some(RecoveryStatus::new(status, now_ms));
        }
    }

    fn model_selected(&mut self, model: String) {
        if model != self.options.model {
            self.statusline.model_changed();
        }
        self.options.model = model;
    }

    fn stats_requested(&mut self) {
        let metrics = self.metrics;
        let body = format!(
            "ansi_bytes={}, redraws={}, debounced_resizes={}, footer_updates=0, stream_chunks=0",
            metrics.ansi_bytes, metrics.full_redraws, metrics.debounced_resizes
        );
        self.push_entry(Entry::Notice(Notice::new(
            NoticeTone::Neutral,
            "stats",
            body,
        )));
    }

    fn compaction_activity(&mut self, activity: CompactionActivity) {
        let now_ms = self.now_ms();
        match activity {
            CompactionActivity::Preparing => {
                self.compaction = Some(CompactionStatus::preparing(self.compaction, now_ms));
            }
            CompactionActivity::Summarizing => {
                if let Some(status) = &mut self.compaction {
                    status.summarizing();
                }
            }
            CompactionActivity::Compacted => {
                self.compaction = None;
                self.promote_next();
            }
            CompactionActivity::Ended(_) if self.turn_compaction_running() => {}
            CompactionActivity::Ended(end) => {
                self.compaction = Some(CompactionStatus::ended(end, now_ms));
                self.promote_next();
            }
        }
    }

    fn turn_compaction_running(&self) -> bool {
        self.compaction
            .is_some_and(|status| status.running() && status.turn().is_some())
    }

    fn turn_compaction(&mut self, turn_id: TurnId, activity: CompactionActivity) {
        let now_ms = self.now_ms();
        self.compaction = match activity {
            CompactionActivity::Preparing => {
                Some(CompactionStatus::turn_preparing(turn_id, now_ms))
            }
            CompactionActivity::Summarizing => self.compaction.map(|mut status| {
                status.summarizing();
                status
            }),
            CompactionActivity::Compacted => None,
            CompactionActivity::Ended(end) => Some(CompactionStatus::ended(end, now_ms)),
        };
    }

    fn settle_turn_compaction(&mut self, turn_id: TurnId, outcome: TurnOutcome) {
        let unsettled = self
            .compaction
            .is_some_and(|status| status.running() && status.turn() == Some(turn_id));
        if unsettled {
            let end = if outcome == TurnOutcome::Interrupted {
                CompactionEnd::Cancelled
            } else {
                CompactionEnd::Failed
            };
            self.compaction = Some(CompactionStatus::ended(end, self.now_ms()));
        }
    }

    pub(super) fn cancel_compaction(&mut self) {
        let Some(status) = self.compaction.as_mut().filter(|status| status.running()) else {
            return;
        };
        status.stopping();
        self.send(UiCommand::CancelCompaction);
    }

    pub(super) fn dismiss_compaction_feedback(&mut self) -> bool {
        if self.compaction.is_none_or(|status| status.running()) {
            return false;
        }
        self.compaction = None;
        self.mark_dirty();
        true
    }

    pub(super) fn pause_connectivity_wait(&mut self) -> bool {
        let Some(turn) = self.turn.as_mut() else {
            return false;
        };
        if turn.pause_requested {
            return true;
        }
        let waiting = turn
            .recovery
            .as_ref()
            .is_some_and(RecoveryStatus::is_connectivity_wait);
        let Some(turn_id) = turn.turn_id.filter(|_| waiting) else {
            return false;
        };
        turn.pause_requested = true;
        self.send(UiCommand::PauseRecovery { turn_id });
        true
    }

    pub(super) fn refresh_recovery_status(&mut self, now_ms: i64) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let changed = match &mut turn.recovery {
            Some(recovery) if recovery.expired(now_ms) => {
                turn.recovery = None;
                true
            }
            Some(recovery) => recovery.refresh(now_ms),
            None => false,
        };
        if changed {
            self.mark_dirty();
        }
    }

    pub(super) fn expire_compaction_feedback(&mut self, now_ms: i64) {
        if self.compaction.is_some_and(|status| status.expired(now_ms)) {
            self.compaction = None;
            self.mark_dirty();
        }
    }

    fn tool_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::ToolStarted {
                turn_id,
                call_id,
                tool_name,
                description,
            } => {
                self.end_assistant_step(turn_id);
                if let Some(turn) = self.visible_turn(turn_id) {
                    turn.phase = TurnPhase::Running;
                    if !asks_the_user(&tool_name, Some(&description)) {
                        self.transcript.add_tool_row(ToolActivityRow::started(
                            call_id,
                            &tool_name,
                            description,
                        ));
                    }
                }
            }
            UiEvent::ToolRejected {
                turn_id,
                call_id,
                tool_name,
                arguments,
                reason,
                description,
                content,
            } => {
                self.end_assistant_step(turn_id);
                if self.is_visible_turn(turn_id) && !asks_the_user(&tool_name, description.as_ref())
                {
                    let rejected = Rejected {
                        reason,
                        arguments: &arguments,
                        description,
                        content: &content,
                    };
                    self.transcript
                        .add_tool_row(ToolActivityRow::rejected(call_id, &tool_name, rejected));
                }
            }
            UiEvent::ToolFinished {
                turn_id,
                call_id,
                arguments,
                status,
                content,
                process,
                status_detail,
                file_change,
                ..
            } => {
                if self.is_visible_turn(turn_id)
                    && let Some(row) = self.transcript.tool_row_mut(&call_id)
                {
                    row.finish(&Finished {
                        arguments: &arguments,
                        status,
                        content: &content,
                        process,
                        status_detail,
                        file_change,
                    });
                }
            }
            UiEvent::SubagentStatus {
                turn_id,
                call_id,
                status,
            } => {
                if self.is_visible_turn(turn_id)
                    && let Some(row) = self.transcript.tool_row_mut(&call_id)
                {
                    row.report_child(&status);
                }
            }
            UiEvent::ToolDeferred {
                turn_id,
                call_id,
                deferral,
            } => {
                if self.is_visible_turn(turn_id)
                    && let Some(row) = self.transcript.tool_row_mut(&call_id)
                {
                    row.defer(deferral);
                }
            }
            UiEvent::ApprovalFeedback { turn_id, text } if self.is_visible_turn(turn_id) => {
                self.push_entry(Entry::UserTurn { text });
            }
            _ => {}
        }
    }

    fn conversation_cleared(&mut self, first_kept_prompt: u64) {
        self.outstanding
            .retain(|submission| submission.sequence >= first_kept_prompt);
        for submission in &mut self.outstanding {
            if submission.state == SubmissionState::Active {
                submission.state = SubmissionState::Queued;
            }
        }
        self.turn = None;
        self.compaction = None;
        self.kept_recovery = None;
        self.statusline.conversation_cleared();
        self.dismiss_approval();
        self.dismiss_question();
        self.composer.reset_for_session();
        self.start_fresh_transcript(FreshScreen::KeepScrollback);
        self.promote_next();
    }

    pub(super) fn is_visible_turn(&self, turn_id: TurnId) -> bool {
        self.turn
            .as_ref()
            .is_some_and(|turn| turn.turn_id == Some(turn_id))
    }

    fn visible_turn(&mut self, turn_id: TurnId) -> Option<&mut ActiveTurn> {
        self.turn
            .as_mut()
            .filter(|turn| turn.turn_id == Some(turn_id))
    }

    fn turn_started(&mut self, turn_id: TurnId) {
        self.statusline.turn_started();
        let Some(index) = self
            .outstanding
            .iter()
            .position(|submission| submission.turn_id.is_none())
        else {
            return;
        };
        self.foreground(super::ForegroundState::Working, None);
        self.outstanding[index].turn_id = Some(turn_id);
        match self.outstanding[index].state {
            SubmissionState::Cancelled => self.send(UiCommand::Cancel { turn_id }),
            SubmissionState::Active => {
                if let Some(turn) = &mut self.turn
                    && turn.turn_id.is_none()
                {
                    turn.turn_id = Some(turn_id);
                }
            }
            SubmissionState::Held => self.resume_held_prompt(index),
            SubmissionState::Queued => {}
        }
    }

    fn assistant_text(&mut self, text: &str) -> Streamed {
        let Some(turn) = &mut self.turn else {
            return Streamed::Unchanged;
        };
        let phase_changed = turn.phase != TurnPhase::Generating;
        turn.phase = TurnPhase::Generating;
        let before = turn.tokens.progress();
        turn.tokens.consume_content(text);
        let counted = turn.tokens.progress() != before;
        if self.present_assistant(text) || phase_changed {
            Streamed::Shown
        } else {
            Streamed::counted(counted)
        }
    }

    fn restart_assistant(&mut self, text: &str) {
        let Some(turn) = &mut self.turn else {
            return;
        };
        let notice = text.trim_start_matches('\n');
        let mut events = Vec::new();
        turn.markdown
            .push(&text[..text.len() - notice.len()], &mut events);
        turn.markdown.flush(&mut events);
        turn.markdown = MarkdownProcessor::with_completions(Completions::ALL);
        self.transcript.append_assistant(events, &self.theme);
        self.present_assistant(notice);
    }

    fn present_assistant(&mut self, text: &str) -> bool {
        let Some(turn) = &mut self.turn else {
            return false;
        };
        let Some(text) = turn.leading_whitespace.release(text) else {
            return false;
        };
        let trailing = text.len() - text.trim_end_matches('\n').len();
        turn.step_break = if trailing == text.len() {
            turn.step_break
                .map(|missing| missing.saturating_sub(trailing))
        } else {
            Some(PARAGRAPH_BREAK.len().saturating_sub(trailing))
        };
        let mut events = Vec::new();
        turn.markdown.push(&text, &mut events);
        let shown = !events.is_empty();
        self.transcript.append_assistant(events, &self.theme);
        shown
    }

    fn end_assistant_step(&mut self, turn_id: TurnId) {
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| turn.turn_id == Some(turn_id))
        else {
            return;
        };
        turn.leading_whitespace = LeadingWhitespace::default();
        let Some(missing) = turn.step_break.take() else {
            return;
        };
        let mut events = Vec::new();
        turn.markdown.flush(&mut events);
        turn.markdown.push(&PARAGRAPH_BREAK[..missing], &mut events);
        self.transcript.append_assistant(events, &self.theme);
    }

    fn operational_text(&mut self, text: &str) {
        let Some(turn) = &mut self.turn else {
            return;
        };
        let mut events = Vec::new();
        turn.markdown.flush(&mut events);
        turn.markdown.push(text, &mut events);
        if !text.ends_with('\n') {
            turn.markdown.push("\n", &mut events);
        }
        self.transcript.append_assistant(events, &self.theme);
    }

    fn turn_finished(&mut self, turn_id: TurnId, outcome: TurnOutcome) {
        let Some(index) = self
            .outstanding
            .iter()
            .position(|submission| submission.turn_id == Some(turn_id))
        else {
            return;
        };
        self.foreground(super::ForegroundState::Idle, None);
        let was_visible = self
            .outstanding
            .remove(index)
            .is_some_and(|submission| submission.state == SubmissionState::Active);
        if was_visible && let Some(turn) = self.turn.take() {
            self.dismiss_approval();
            self.dismiss_question();
            self.settle_turn_compaction(turn_id, outcome);
            self.finish_visible_turn(turn, outcome);
        }
        self.promote_next();
    }

    fn finish_visible_turn(&mut self, mut turn: ActiveTurn, outcome: TurnOutcome) {
        self.kept_recovery = turn.recovery.take().filter(RecoveryStatus::is_terminal);
        if outcome != TurnOutcome::Interrupted {
            let mut events = Vec::new();
            turn.markdown.flush(&mut events);
            self.transcript.append_assistant(events, &self.theme);
        }
        let tools_settled = self.transcript.abandon_active_tools(outcome);
        match outcome {
            TurnOutcome::Completed => {
                let duration_ms = u64::try_from(self.now_ms() - turn.started_ms).unwrap_or(0);
                self.push_entry(Entry::TurnSummary {
                    duration_ms,
                    progress: turn.tokens.progress(),
                });
            }
            TurnOutcome::Interrupted if tools_settled => {}
            TurnOutcome::Interrupted => self.push_entry(Entry::Cancellation),
            TurnOutcome::Failed => {
                if let Some(text) = turn.failure {
                    self.push_entry(Entry::TurnFailure { text });
                }
            }
        }
    }

    pub(super) fn promote_next(&mut self) {
        if self.working() {
            return;
        }
        let now_ms = self.now_ms();
        let Some(submission) = self
            .outstanding
            .iter_mut()
            .find(|submission| submission.state != SubmissionState::Cancelled)
        else {
            return;
        };
        if submission.state != SubmissionState::Queued {
            return;
        }
        submission.state = SubmissionState::Active;
        let mut turn = ActiveTurn::new(&submission.prompt, now_ms);
        turn.turn_id = submission.turn_id;
        let text = submission.prompt.clone();
        let presented = submission.presented;
        self.turn = Some(turn);
        self.kept_recovery = None;
        if !presented {
            self.push_entry(Entry::UserTurn { text });
        }
    }

    fn recovery_continuing(&mut self, prompt: String, id: u64) {
        let ahead = self
            .outstanding
            .iter()
            .position(|submission| submission.turn_id.is_none())
            .unwrap_or(self.outstanding.len());
        self.displace_unstarted_turn();
        let presented = self.submitted_prompts == self.prompts_before_resume;
        for submission in &mut self.outstanding {
            if submission.sequence >= id {
                submission.sequence += 1;
            }
        }
        self.outstanding.insert(
            ahead,
            Submission {
                prompt,
                state: SubmissionState::Queued,
                turn_id: None,
                sequence: id,
                presented,
            },
        );
        self.submitted_prompts += 1;
        self.promote_next();
    }

    fn displace_unstarted_turn(&mut self) {
        if self.turn.as_ref().is_none_or(|turn| turn.turn_id.is_some()) {
            return;
        }
        let Some(submission) = self
            .outstanding
            .iter_mut()
            .find(|submission| submission.state == SubmissionState::Active)
        else {
            return;
        };
        submission.state = SubmissionState::Queued;
        submission.presented = false;
        self.turn = None;
    }

    pub(super) fn cancel_visible_turn(&mut self) {
        self.cancel_visible_turn_noting(Entry::Cancellation);
    }

    pub(super) fn cancel_visible_turn_noting(&mut self, entry: Entry) {
        let Some(turn) = self.turn.take() else {
            return;
        };
        if let Some(turn_id) = turn.turn_id {
            self.settle_turn_compaction(turn_id, TurnOutcome::Interrupted);
        }
        self.reveal_pending_approval_call();
        self.dismiss_approval();
        self.dismiss_question();
        let mut started = None;
        if let Some(submission) = self
            .outstanding
            .iter_mut()
            .find(|submission| submission.state == SubmissionState::Active)
        {
            submission.state = SubmissionState::Cancelled;
            started = submission.turn_id;
        }
        if let Some(turn_id) = started {
            self.send(UiCommand::Cancel { turn_id });
        }
        if !self.transcript.cancel_active_tools() {
            self.push_entry(entry);
        }
        self.promote_next();
    }
}

fn asks_the_user(tool_name: &str, description: Option<&CallDescription>) -> bool {
    description.map_or(tool_name == ASK_USER_QUESTION, |description| {
        description.activity == ToolActivity::Ask
    })
}

#[cfg(test)]
mod recovery_rows;

#[cfg(test)]
mod statusline_rows;

#[cfg(test)]
mod tool_rows;

#[cfg(test)]
mod tests {
    use ofx_contract::{
        ActionLabel, CallDescription, CompactionActivity, CompactionEnd, Concurrency, HistoryEntry,
        Notice, NoticeTone, QuestionBatchEntry, QuestionOption, QuestionRequest, RequestId,
        ToolActivity, ToolCallId, ToolEffect, ToolResultStatus, TurnId, TurnOutcome, UiCommand,
        UiEvent, Usage,
    };

    use super::super::Opening;
    use super::super::SlashCommandSpec;
    use super::super::test_shell::TestShell;
    use crate::input::{PasteOutcome, PasteOwner};

    fn text(turn: u64, text: &str) -> UiEvent {
        UiEvent::AssistantText {
            turn_id: TurnId::new(turn),
            text: text.to_owned(),
        }
    }

    fn started(turn: u64) -> UiEvent {
        UiEvent::TurnStarted {
            turn_id: TurnId::new(turn),
        }
    }

    fn finished(turn: u64, outcome: TurnOutcome) -> UiEvent {
        UiEvent::TurnFinished {
            turn_id: TurnId::new(turn),
            outcome,
        }
    }

    #[test]
    fn a_resumed_session_opens_on_its_saved_transcript_instead_of_the_welcome() {
        let history = vec![
            HistoryEntry::Notice(Notice::new(
                NoticeTone::Neutral,
                "session resumed",
                "fix the build",
            )),
            HistoryEntry::User("fix the build".to_owned()),
            HistoryEntry::Assistant("Looking.".to_owned()),
            HistoryEntry::Assistant("Fixed **it**.".to_owned()),
            HistoryEntry::User("again".to_owned()),
            HistoryEntry::Assistant(String::new()),
            HistoryEntry::Cancelled,
        ];
        let mut test =
            TestShell::start_with(|options| options.opening = Opening::Transcript(history));
        let screen = test.screen();
        assert!(!screen.contains("Run /help"), "{screen}");
        assert!(
            screen.contains(
                "* session resumed: fix the build\n\n┃ fix the build\n\n  Looking.\n\n  Fixed it.\n\n┃ again\n\n■ Cancelled · What can oh-fx do differently?"
            ),
            "{screen}"
        );
    }

    fn streaming() -> TestShell {
        let mut test = TestShell::start();
        test.submit("go");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test.screen();
        test
    }

    fn reasoning(text: &str) -> UiEvent {
        UiEvent::ReasoningText {
            turn_id: TurnId::new(1),
            text: text.to_owned(),
        }
    }

    #[test]
    fn token_counts_alone_redraw_at_most_every_50_ms() {
        let mut test = streaming();
        let drawn = test.shell.frame.drawn_ms;
        for _ in 0..20 {
            test.deliver(reasoning(&"think ".repeat(20)));
        }
        assert!(!test.shell.frame_due(drawn + 49));
        assert!(test.shell.frame_due(drawn + 50));
        test.advance(50);
        let written = test.written();
        assert_eq!(written.matches('↓').count(), 1, "{written:?}");
        test.deliver(reasoning(" "));
        let drawn = test.shell.frame.drawn_ms;
        assert!(!test.shell.frame_due(drawn));
        assert_eq!(test.shell.token_redraw_ms(), None);
        test.deliver(UiEvent::AssistantText {
            turn_id: TurnId::new(1),
            text: "partial".to_owned(),
        });
        let written = test.written();
        assert!(written.contains("Generating"), "{written:?}");
        test.deliver(UiEvent::AssistantText {
            turn_id: TurnId::new(1),
            text: " words".repeat(20),
        });
        let drawn = test.shell.frame.drawn_ms;
        assert!(!test.shell.frame_due(drawn + 49));
        assert!(test.shell.frame_due(drawn + 50));
        test.deliver(UiEvent::AssistantText {
            turn_id: TurnId::new(1),
            text: " done\n\nnext\n".to_owned(),
        });
        assert!(test.written().contains("done"));
        test.deliver(UiEvent::UsageReported {
            turn_id: TurnId::new(1),
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(999),
            },
            context_window: None,
        });
        assert!(test.written().contains("↓999"));
    }

    #[test]
    fn token_counts_draw_at_once_while_no_activity_row_is_on_screen() {
        let mut test = streaming();
        test.deliver(UiEvent::QuestionRequested {
            turn_id: TurnId::new(1),
            request: QuestionRequest {
                id: RequestId::new(2),
                entries: vec![QuestionBatchEntry {
                    question: "Continue?".to_owned(),
                    options: vec![QuestionOption {
                        label: "Yes".to_owned(),
                        description: None,
                    }],
                }],
            },
        });
        test.screen();
        let drawn = test.shell.frame.drawn_ms;
        assert!(!test.shell.frame_due(drawn));
        test.deliver(reasoning(&"think ".repeat(20)));
        assert!(test.shell.frame_due(drawn));
        assert_eq!(test.shell.token_redraw_ms(), None);
    }

    #[test]
    fn blocked_frames_keep_token_updates_without_an_overdue_deadline() {
        let mut test = streaming();
        let now_ms = test.shell.now_ms();
        test.shell.handle_resize_signal(now_ms);
        test.deliver(reasoning(&"think ".repeat(20)));
        assert!(test.written().is_empty());
        test.advance(50);
        assert!(test.written().is_empty());
        let mid_resize_ms = now_ms + 50;
        assert!(
            test.shell
                .next_deadline_ms(mid_resize_ms)
                .is_some_and(|due_ms| due_ms > mid_resize_ms)
        );
        test.advance(50);
        test.draining(|shell| {
            let now_ms = shell.now_ms();
            shell.apply_pending_resize(now_ms);
        });
        assert!(test.written().contains('↓'));
    }

    fn compaction(activity: CompactionActivity) -> UiEvent {
        UiEvent::CompactionActivity { activity }
    }

    fn turn_compaction(turn: u64, activity: CompactionActivity) -> UiEvent {
        UiEvent::TurnCompaction {
            turn_id: TurnId::new(turn),
            activity,
        }
    }

    fn compacting_turn() -> TestShell {
        let mut test = TestShell::start();
        test.submit("go");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test.screen();
        test.advance(3_000);
        test.deliver(turn_compaction(1, CompactionActivity::Preparing));
        test
    }

    #[test]
    fn an_automatic_compaction_shows_on_the_turn_clock_until_it_succeeds() {
        let mut test = compacting_turn();
        let screen = test.screen();
        assert!(screen.contains("• Preparing compaction (3s)"), "{screen}");
        assert!(!screen.contains("Thinking"), "{screen}");
        test.deliver(turn_compaction(1, CompactionActivity::Summarizing));
        assert!(test.screen().contains("• Compacting (3s)"));
        test.deliver(turn_compaction(2, CompactionActivity::Compacted));
        assert!(test.screen().contains("• Compacting (3s)"));
        test.deliver(turn_compaction(1, CompactionActivity::Compacted));
        let screen = test.screen();
        assert!(screen.contains("• Thinking (3s)"), "{screen}");
        assert!(!screen.contains("ompact"), "{screen}");
        assert_eq!(test.sent().len(), 1);
    }

    #[test]
    fn a_failed_automatic_compaction_keeps_its_feedback_after_the_turn_until_the_next_prompt() {
        let mut test = compacting_turn();
        test.deliver(turn_compaction(
            1,
            CompactionActivity::Ended(CompactionEnd::Failed),
        ));
        test.deliver(finished(1, TurnOutcome::Failed));
        let screen = test.screen();
        assert!(
            screen.contains("Compaction failed. Try /compact again."),
            "{screen}"
        );
        assert!(!test.shell.working());
        test.submit("next");
        assert!(!test.screen().contains("Compaction failed"));
    }

    #[test]
    fn cancelling_a_turn_during_its_compaction_reports_the_compaction_cancelled() {
        let mut test = compacting_turn();
        test.type_bytes(b"\x03");
        test.step();
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::Cancel {
                turn_id: TurnId::new(1)
            })
        );
        test.deliver(turn_compaction(
            1,
            CompactionActivity::Ended(CompactionEnd::Failed),
        ));
        let screen = test.screen();
        assert!(
            screen.contains("Compaction cancelled. Try /compact again when ready."),
            "{screen}"
        );
        test.type_bytes(b"\x1b");
        test.step();
        test.advance(50);
        test.step();
        assert!(!test.screen().contains("Compaction cancelled"));
    }

    #[test]
    fn a_rejected_compact_command_leaves_a_running_automatic_compaction_alone() {
        let mut test = compacting_turn();
        test.deliver(turn_compaction(1, CompactionActivity::Summarizing));
        test.submit("/compact");
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::RunCommand {
                text: "/compact".to_owned()
            })
        );
        test.deliver(compaction(CompactionActivity::Ended(CompactionEnd::Busy)));
        let screen = test.screen();
        assert!(screen.contains("• Compacting (3s)"), "{screen}");
        assert!(!screen.contains("Wait for the active work"), "{screen}");
        test.advance(1_500);
        test.step();
        let screen = test.screen();
        assert!(screen.contains("• Compacting (5s)"), "{screen}");
        assert!(!screen.contains("Thinking"), "{screen}");
        test.deliver(finished(1, TurnOutcome::Interrupted));
        let screen = test.screen();
        assert!(
            screen.contains("Compaction cancelled. Try /compact again when ready."),
            "{screen}"
        );
    }

    #[test]
    fn a_turn_that_ends_during_its_compaction_settles_the_compaction() {
        for (outcome, feedback) in [
            (
                TurnOutcome::Interrupted,
                "Compaction cancelled. Try /compact again when ready.",
            ),
            (
                TurnOutcome::Failed,
                "Compaction failed. Try /compact again.",
            ),
        ] {
            let mut test = compacting_turn();
            test.deliver(finished(1, outcome));
            let screen = test.screen();
            assert!(screen.contains(feedback), "{screen}");
        }
        let mut test = compacting_turn();
        test.deliver(turn_compaction(1, CompactionActivity::Compacted));
        test.deliver(finished(1, TurnOutcome::Completed));
        assert!(!test.screen().contains("ompaction"));
    }

    #[test]
    fn manual_compaction_shows_its_phases_and_holds_prompts_until_it_ends() {
        let mut test = TestShell::start();
        test.submit("/compact");
        test.deliver(compaction(CompactionActivity::Preparing));
        let screen = test.screen();
        assert!(screen.contains("• Preparing compaction (0s)"), "{screen}");
        test.deliver(compaction(CompactionActivity::Summarizing));
        test.advance(2_000);
        let screen = test.screen();
        assert!(screen.contains("• Compacting (2s)"), "{screen}");
        test.submit("after");
        let screen = test.screen();
        assert!(screen.contains("┋ after"), "{screen}");
        assert!(!screen.contains("┃ after"), "{screen}");
        test.deliver(compaction(CompactionActivity::Compacted));
        let screen = test.screen();
        assert!(!screen.contains("compact"), "{screen}");
        assert!(screen.contains("┃ after"), "{screen}");
        assert!(screen.contains("• Thinking (0s)"), "{screen}");
        assert_eq!(
            test.sent(),
            [
                UiCommand::RunCommand {
                    text: "/compact".to_owned()
                },
                UiCommand::Submit {
                    prompt: "after".to_owned(),
                    skills: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn transient_compaction_feedback_expires_after_a_moment() {
        let mut test = TestShell::start();
        test.deliver(compaction(CompactionActivity::Ended(
            CompactionEnd::NothingToCompact,
        )));
        let screen = test.screen();
        assert!(screen.contains("No context to compact."), "{screen}");
        test.advance(1_499);
        test.draining(|shell| {
            let now_ms = shell.now_ms();
            shell.expire_compaction_feedback(now_ms.min(1_499));
        });
        assert!(test.screen().contains("No context to compact."));
        test.advance(1);
        test.step();
        let screen = test.screen();
        assert!(!screen.contains("No context to compact."), "{screen}");
    }

    #[test]
    fn busy_feedback_replaces_the_turn_activity_until_it_expires() {
        let mut test = TestShell::start();
        test.submit("slow");
        test.deliver(started(1));
        test.deliver(compaction(CompactionActivity::Ended(CompactionEnd::Busy)));
        let screen = test.screen();
        assert!(
            screen.contains("Wait for the active work to finish before compacting context."),
            "{screen}"
        );
        assert!(!screen.contains("Thinking"), "{screen}");
        test.advance(1_500);
        test.step();
        let screen = test.screen();
        assert!(!screen.contains("Wait for the active work"), "{screen}");
        assert!(screen.contains("Thinking (1s)"), "{screen}");
    }

    #[test]
    fn lasting_compaction_feedback_stays_until_the_next_submission_or_escape() {
        for (end, label) in [
            (
                CompactionEnd::Failed,
                "Compaction failed. Try /compact again.",
            ),
            (
                CompactionEnd::Cancelled,
                "Compaction cancelled. Try /compact again when ready.",
            ),
            (
                CompactionEnd::ContextTooLarge,
                "Context is too large to compact. Choose a model with a larger context window.",
            ),
        ] {
            let mut test = TestShell::start();
            test.deliver(compaction(CompactionActivity::Ended(end)));
            test.advance(60_000);
            test.draining(|shell| {
                let now_ms = shell.now_ms();
                shell.expire_compaction_feedback(now_ms);
            });
            let screen = test.screen();
            assert!(screen.contains(label), "{screen}");
            test.submit("next");
            assert!(!test.screen().contains(label));
            test.deliver(compaction(CompactionActivity::Ended(end)));
            assert!(test.screen().contains(label));
            test.type_bytes(b"\x1b[27u");
            test.step();
            let screen = test.screen();
            assert!(!screen.contains(label), "{screen}");
        }
    }

    fn with_compact() -> TestShell {
        TestShell::start_with(|options| {
            options.commands.push(SlashCommandSpec {
                command: "/compact".to_owned(),
                aliases: Vec::new(),
                description: String::new(),
                help_entry: "/compact".to_owned(),
                takes_arguments: false,
                category: 0,
                compacts: true,
            });
        })
    }

    #[test]
    fn a_prompt_sent_before_compaction_starts_waits_and_an_interrupt_stops_the_compaction() {
        for keys in [&b"\x03"[..], b"\x1b[27u\x1b[27u"] {
            let mut test = with_compact();
            test.submit("/compact");
            test.submit("after");
            let screen = test.screen();
            assert!(screen.contains("┋ after"), "{screen}");
            assert!(!screen.contains("┃ after"), "{screen}");
            test.deliver(compaction(CompactionActivity::Preparing));
            test.deliver(compaction(CompactionActivity::Summarizing));
            test.type_bytes(keys);
            test.step();
            assert_eq!(
                test.sent(),
                [
                    UiCommand::RunCommand {
                        text: "/compact".to_owned()
                    },
                    UiCommand::Submit {
                        prompt: "after".to_owned(),
                        skills: Vec::new(),
                    },
                    UiCommand::CancelCompaction,
                ],
                "{keys:?}"
            );
            let screen = test.screen();
            assert!(screen.contains("• Stopping compaction"), "{screen}");
            assert!(screen.contains("┋ after"), "{screen}");
            test.deliver(compaction(CompactionActivity::Ended(
                CompactionEnd::Cancelled,
            )));
            let screen = test.screen();
            assert!(screen.contains("┃ after"), "{screen}");
            assert!(screen.contains("Compaction cancelled."), "{screen}");
        }
    }

    #[test]
    fn a_compaction_with_nothing_to_compact_releases_the_prompts_held_for_it() {
        let mut test = with_compact();
        test.submit("/compact");
        test.submit("held");
        assert!(test.screen().contains("┋ held"));
        test.deliver(compaction(CompactionActivity::Ended(
            CompactionEnd::NothingToCompact,
        )));
        let screen = test.screen();
        assert!(screen.contains("┃ held"), "{screen}");
        assert!(screen.contains("No context to compact."), "{screen}");
        let mut unknown = with_compact();
        unknown.submit("/compact now");
        unknown.submit("runs");
        let screen = unknown.screen();
        assert!(screen.contains("┃ runs"), "{screen}");
    }

    #[test]
    fn interrupting_a_compaction_asks_the_agent_to_stop_it() {
        for keys in [&b"\x03"[..], b"\x1b[27u\x1b[27u"] {
            let mut test = TestShell::start();
            test.deliver(compaction(CompactionActivity::Preparing));
            test.deliver(compaction(CompactionActivity::Summarizing));
            test.type_bytes(keys);
            test.step();
            assert_eq!(test.sent(), [UiCommand::CancelCompaction], "{keys:?}");
            let screen = test.screen();
            assert!(screen.contains("• Stopping compaction (0s)"), "{screen}");
            test.deliver(compaction(CompactionActivity::Summarizing));
            assert!(test.screen().contains("Stopping compaction"));
            test.deliver(compaction(CompactionActivity::Ended(
                CompactionEnd::Cancelled,
            )));
            let screen = test.screen();
            assert!(
                screen.contains("Compaction cancelled. Try /compact again when ready."),
                "{screen}"
            );
        }
    }

    #[test]
    fn clearing_the_conversation_dismisses_compaction_feedback() {
        let mut test = TestShell::start();
        test.deliver(compaction(CompactionActivity::Ended(CompactionEnd::Failed)));
        test.deliver(UiEvent::ConversationCleared {
            first_kept_prompt: 0,
        });
        let screen = test.screen();
        assert!(!screen.contains("Compaction failed"), "{screen}");
    }

    #[test]
    fn stats_report_the_bytes_redraws_and_resizes_the_renderer_counted() {
        let mut test = TestShell::start();
        let mut frames = String::new();
        frames += &test.written();
        test.submit("/stats");
        assert_eq!(
            test.sent(),
            [UiCommand::RunCommand {
                text: "/stats".to_owned()
            }]
        );
        test.resize(24, 120);
        frames += &test.written();
        test.resize(24, 120);
        frames += &test.written();
        test.deliver(UiEvent::StatsRequested);
        let translated_line_feeds = frames.matches('\n').count();
        let ansi_bytes = frames.len() - translated_line_feeds;
        let screen = test.screen();
        assert!(
            screen.contains(&format!(
                "* stats: ansi_bytes={ansi_bytes}, redraws=1, debounced_resizes=2, footer_updates=0, stream_chunks=0"
            )),
            "{screen}"
        );
    }

    #[test]
    fn operational_notices_that_end_their_line_add_no_blank_row() {
        let mut test = TestShell::start();
        test.submit("one");
        test.deliver(started(1));
        test.deliver(text(1, "Working."));
        test.deliver(UiEvent::Operational {
            turn_id: TurnId::new(1),
            text: "Agent step limit reached; continue with a follow-up prompt if needed.\n"
                .to_owned(),
        });
        test.deliver(finished(1, TurnOutcome::Failed));
        let screen = test.screen();
        assert!(
            screen.contains("  Working.\n  Agent step limit reached; continue with a follow-up prompt if needed.\n\n┃"),
            "{screen}"
        );
    }

    #[test]
    fn requested_skill_notices_show_their_rows_under_the_prompt() {
        let mut test = TestShell::start();
        test.submit("$review and $release");
        test.deliver(started(1));
        test.deliver(UiEvent::Notice {
            notice: Notice::new(
                NoticeTone::Warning,
                "",
                "Requested skills \u{b7} 1 loaded \u{b7} 1 failed (ctrl+o for details)\n\u{251c} Loaded skill review\n\u{2514} Could not load release",
            ),
        });
        test.deliver(text(1, "Reviewed."));
        test.deliver(finished(1, TurnOutcome::Completed));
        let screen = test.screen();
        assert!(
            screen.contains(
                "! Requested skills \u{b7} 1 loaded \u{b7} 1 failed (ctrl+o for details)\n\u{251c} Loaded skill review\n\u{2514} Could not load release\n"
            ),
            "{screen}"
        );
        let prompt = screen.find("$review and $release").unwrap();
        let notice = screen.find("! Requested skills").unwrap();
        let reply = screen.find("Reviewed.").unwrap();
        assert!(prompt < notice && notice < reply, "{screen}");
    }

    #[test]
    fn system_notices_show_as_neutral_notice_rows_in_the_turn() {
        let mut test = TestShell::start();
        test.submit("fix it");
        test.deliver(started(1));
        test.deliver(UiEvent::SystemNotice {
            text: "Repeated shell validation failures stopped the tool loop.".to_owned(),
        });
        test.deliver(finished(1, TurnOutcome::Completed));
        let screen = test.screen();
        assert!(
            screen.contains(
                "┃ fix it\n\n* system: Repeated shell validation failures stopped the tool loop.\n"
            ),
            "{screen}"
        );
    }

    fn tool_started(turn: u64, call: &str) -> UiEvent {
        UiEvent::ToolStarted {
            turn_id: TurnId::new(turn),
            call_id: ToolCallId::new(call),
            tool_name: "read_file".to_owned(),
            description: CallDescription {
                title: "Reading README.md".to_owned(),
                label: Some(ActionLabel {
                    active: "Reading",
                    completed: "Read",
                    target: "README.md".to_owned(),
                }),
                activity: ToolActivity::Read,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Parallel,
            },
        }
    }

    fn tool_finished(turn: u64, call: &str) -> UiEvent {
        UiEvent::ToolFinished {
            turn_id: TurnId::new(turn),
            call_id: ToolCallId::new(call),
            tool_name: "read_file".to_owned(),
            arguments: "{}".to_owned(),
            status: ToolResultStatus::Success,
            content: String::new(),
            command_result: None,
            process: None,
            status_detail: None,
            file_change: None,
        }
    }

    #[test]
    fn text_before_a_tool_step_ends_its_paragraph_before_the_next_step() {
        for (before, after) in [
            ("I will read it.", "It describes a service."),
            ("I will read it.\n", "It describes a service."),
            ("I will read it.\n\n", "It describes a service."),
        ] {
            let mut test = TestShell::start();
            test.submit("one");
            test.deliver(started(1));
            test.deliver(text(1, before));
            test.deliver(tool_started(1, "call-1"));
            test.deliver(tool_started(1, "call-2"));
            test.deliver(tool_finished(1, "call-1"));
            test.deliver(tool_finished(1, "call-2"));
            test.deliver(text(1, after));
            test.deliver(finished(1, TurnOutcome::Completed));
            let screen = test.screen();
            assert!(
                screen.contains(
                    "  I will read it.\n\n● 2 tool calls · 2 read\n├ Read README.md\n└ Read README.md\n\n  It describes a service.\n"
                ),
                "{before:?}\n{screen}"
            );
        }
    }

    #[test]
    fn a_tool_step_without_text_before_it_adds_no_blank_rows() {
        let mut test = TestShell::start();
        test.submit("one");
        test.deliver(started(1));
        test.deliver(tool_started(1, "call-1"));
        test.deliver(tool_finished(1, "call-1"));
        test.deliver(text(1, "It describes a service."));
        test.deliver(finished(1, TurnOutcome::Completed));
        let screen = test.screen();
        assert!(
            screen.contains(
                "┃ one\n\n● 1 tool call · 1 read\n└ Read README.md\n\n  It describes a service.\n"
            ),
            "{screen}"
        );
    }

    #[test]
    fn a_prompt_submitted_right_after_clear_stays_visible() {
        let mut test = TestShell::start();
        test.submit("one");
        test.deliver(started(1));
        test.deliver(text(1, "First answer.\n"));
        test.deliver(finished(1, TurnOutcome::Completed));
        test.submit("/clear");
        test.submit("two");
        test.deliver(UiEvent::ConversationCleared {
            first_kept_prompt: 1,
        });
        test.deliver(started(2));
        test.deliver(text(2, "Second answer.\n"));
        test.deliver(finished(2, TurnOutcome::Completed));
        let screen = test.screen();
        assert!(screen.contains("┃ two"), "{screen}");
        assert!(screen.contains("Second answer."), "{screen}");
        assert!(!screen.contains("First answer."), "{screen}");
        assert_eq!(
            test.sent(),
            [
                UiCommand::Submit {
                    prompt: "one".to_owned(),
                    skills: Vec::new(),
                },
                UiCommand::RunCommand {
                    text: "/clear".to_owned()
                },
                UiCommand::Submit {
                    prompt: "two".to_owned(),
                    skills: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn a_continued_recovery_streams_under_its_already_shown_prompt() {
        let mut test = TestShell::start();
        test.deliver(UiEvent::SessionResumed {
            history: vec![HistoryEntry::User("fix the build".to_owned())],
        });
        test.deliver(UiEvent::RecoveryContinuing {
            prompt: "fix the build".to_owned(),
            id: 0,
        });
        test.deliver(started(1));
        test.deliver(text(1, "Build fixed.\n"));
        test.deliver(finished(1, TurnOutcome::Completed));
        let screen = test.screen();
        assert_eq!(screen.matches("┃ fix the build").count(), 1, "{screen}");
        assert!(screen.contains("Build fixed."), "{screen}");
        assert!(test.sent().is_empty());
        test.submit("next");
        test.deliver(started(2));
        test.deliver(text(2, "Next answer.\n"));
        test.deliver(finished(2, TurnOutcome::Completed));
        let screen = test.screen();
        assert!(screen.contains("┃ next"), "{screen}");
        assert!(screen.contains("Next answer."), "{screen}");
    }

    fn in_order(screen: &str, pieces: &[&str]) -> bool {
        let mut rest = screen;
        pieces.iter().all(|piece| {
            rest.find(piece)
                .map(|at| rest = &rest[at + piece.len()..])
                .is_some()
        })
    }

    #[test]
    fn a_resumed_turn_shows_its_saved_summary_row_after_the_reply() {
        let mut test = TestShell::start();
        test.deliver(UiEvent::SessionResumed {
            history: vec![
                HistoryEntry::User("fix the build".to_owned()),
                HistoryEntry::Assistant("Fixed.".to_owned()),
                HistoryEntry::TurnSummary(ofx_contract::TurnSummary {
                    turn_duration_ms: 3500,
                    token_progress: ofx_contract::TurnTokenProgress {
                        input_tokens: 1234,
                        output_tokens: 340,
                        ..ofx_contract::TurnTokenProgress::default()
                    },
                    ..ofx_contract::TurnSummary::default()
                }),
            ],
        });
        let screen = test.screen();
        assert!(
            in_order(&screen, &["┃ fix the build", "Fixed.", "  3s (↑1.2k ↓340)"]),
            "{screen}"
        );
    }

    #[test]
    fn a_resumed_history_past_the_retention_cap_reaches_the_terminal_whole_and_keeps_its_tail() {
        let mut test = TestShell::start();
        let marker = |index: usize| format!("marker {index:04} {}", "z".repeat(150));
        let history = (0..8000)
            .map(|index| {
                HistoryEntry::Notice(Notice::new(NoticeTone::Neutral, "history", marker(index)))
            })
            .collect();
        test.deliver(UiEvent::SessionResumed { history });
        let published = test.written();
        assert!(published.contains("marker 0000"));
        assert!(published.contains("marker 7999"));
        test.draining(super::super::Shell::replay);
        let replayed = test.written();
        assert!(!replayed.contains("marker 0000"));
        assert!(replayed.contains("marker 7999"));
    }

    fn resumed_with_typeahead() -> TestShell {
        let mut test = TestShell::start();
        test.deliver(UiEvent::SessionResumed {
            history: vec![HistoryEntry::User("fix the build".to_owned())],
        });
        test.submit("new prompt");
        test
    }

    #[test]
    fn typeahead_that_steers_a_continued_recovery_leaves_the_shell_idle_when_it_finishes() {
        let mut test = resumed_with_typeahead();
        test.deliver(UiEvent::RecoveryContinuing {
            prompt: "fix the build".to_owned(),
            id: 0,
        });
        test.deliver(started(1));
        test.deliver(UiEvent::SteeringApplied {
            turn_id: TurnId::new(1),
            prompt: 1,
            text: "new prompt".to_owned(),
        });
        test.deliver(text(1, "Both done.\n"));
        test.deliver(finished(1, TurnOutcome::Completed));
        assert!(
            test.shell.outstanding.is_empty(),
            "{:?}",
            test.shell.outstanding
        );
        assert!(test.shell.turn.is_none());
        assert!(!test.shell.working());
        let screen = test.screen();
        assert!(
            in_order(&screen, &["┃ fix the build", "┃ new prompt", "Both done."]),
            "{screen}"
        );
    }

    #[test]
    fn a_recovery_that_starts_after_typeahead_runs_first_under_its_own_prompt() {
        let mut test = resumed_with_typeahead();
        test.deliver(UiEvent::RecoveryContinuing {
            prompt: "fix the build".to_owned(),
            id: 0,
        });
        test.deliver(started(1));
        test.deliver(text(1, "Build fixed.\n"));
        test.type_bytes(b"\x03");
        test.step();
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::Cancel {
                turn_id: TurnId::new(1)
            })
        );
        test.deliver(finished(1, TurnOutcome::Interrupted));
        test.deliver(started(2));
        test.deliver(text(2, "New answer.\n"));
        test.deliver(finished(2, TurnOutcome::Completed));
        let screen = test.screen();
        assert!(
            in_order(
                &screen,
                &[
                    "┃ fix the build",
                    "┃ new prompt",
                    "┃ fix the build",
                    "Cancelled",
                    "┃ new prompt",
                    "New answer.",
                ]
            ),
            "{screen}"
        );
        assert!(!test.sent().contains(&UiCommand::Cancel {
            turn_id: TurnId::new(2)
        }));
    }

    #[test]
    fn typeahead_cancelled_before_a_recovery_starts_stays_cancelled_behind_it() {
        let mut test = resumed_with_typeahead();
        test.type_bytes(b"\x03");
        test.step();
        test.deliver(UiEvent::RecoveryContinuing {
            prompt: "fix the build".to_owned(),
            id: 0,
        });
        test.deliver(started(1));
        test.deliver(text(1, "Build fixed.\n"));
        assert!(!test.sent().contains(&UiCommand::Cancel {
            turn_id: TurnId::new(1)
        }));
        test.deliver(started(2));
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::Cancel {
                turn_id: TurnId::new(2)
            })
        );
        test.deliver(finished(2, TurnOutcome::Interrupted));
        test.deliver(finished(1, TurnOutcome::Completed));
        let screen = test.screen();
        assert!(
            in_order(
                &screen,
                &[
                    "┃ fix the build",
                    "┃ new prompt",
                    "Cancelled",
                    "┃ fix the build",
                    "Build fixed.",
                ]
            ),
            "{screen}"
        );
        test.submit("after");
        test.deliver(started(3));
        test.deliver(text(3, "After answer.\n"));
        test.deliver(finished(3, TurnOutcome::Completed));
        assert!(test.screen().contains("After answer."));
    }

    #[test]
    fn clearing_the_conversation_empties_the_composer_kill_ring() {
        let mut test = TestShell::start();
        let press = |test: &mut TestShell, keys: &[u8]| {
            test.type_bytes(keys);
            test.draining(|shell| shell.step().unwrap());
        };
        press(&mut test, b"before clear\x15");
        test.submit("/clear");
        press(&mut test, b"\x19");
        assert_eq!(test.shell.composer.text(), "before clear");
        press(&mut test, b"\x15");
        assert_eq!(test.shell.composer.kill_ring_text(), "before clear");
        test.deliver(UiEvent::ConversationCleared {
            first_kept_prompt: 0,
        });
        assert_eq!(test.shell.composer.kill_ring_text(), "");
        press(&mut test, b"\x19");
        assert_eq!(test.shell.composer.text(), "");
        press(&mut test, b"after\x15\x19\x19");
        assert_eq!(test.shell.composer.text(), "afterafter");
    }

    #[test]
    fn clearing_the_conversation_numbers_pastes_from_one_again() {
        let mut test = TestShell::start();
        let paste = |test: &mut TestShell| {
            test.shell.handle_paste(PasteOutcome::Text {
                owner: PasteOwner::Composer,
                text: "pasted\n".repeat(200),
            });
            test.shell.composer.text().to_owned()
        };
        assert_eq!(paste(&mut test), "[Pasted text #1, 200 lines]");
        test.shell.submit();
        test.submit("/clear");
        test.deliver(UiEvent::ConversationCleared {
            first_kept_prompt: 1,
        });
        assert_eq!(paste(&mut test), "[Pasted text #1, 200 lines]");
        test.shell.submit();
        test.submit("/clear");
        assert_eq!(paste(&mut test), "[Pasted text #2, 200 lines]");
        test.deliver(UiEvent::ConversationCleared {
            first_kept_prompt: 2,
        });
        assert_eq!(
            paste(&mut test),
            "[Pasted text #2, 200 lines][Pasted text #3, 200 lines]"
        );
    }

    #[test]
    fn clearing_mid_turn_drops_only_the_prompts_sent_before_the_clear() {
        let mut test = TestShell::start();
        test.submit("slow");
        test.deliver(started(1));
        test.deliver(text(1, "partial\n"));
        test.submit("queued");
        test.submit("/clear");
        test.submit("kept");
        test.deliver(finished(1, TurnOutcome::Interrupted));
        test.deliver(UiEvent::ConversationCleared {
            first_kept_prompt: 2,
        });
        test.deliver(started(2));
        test.deliver(text(2, "Kept answer.\n"));
        test.deliver(finished(2, TurnOutcome::Completed));
        let screen = test.screen();
        assert!(screen.contains("┃ kept"), "{screen}");
        assert!(screen.contains("Kept answer."), "{screen}");
        assert!(!screen.contains("queued"), "{screen}");
        assert!(!screen.contains("slow"), "{screen}");
    }
}

#[cfg(test)]
mod wake_tests {
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::Arc;

    use ofx_contract::{
        ApprovalOrigin, ApprovalRequest, ApprovalScope, CallDescription, Concurrency, FileMutation,
        FileMutationState, PathAccess, ProposedFileChange, RequestId, ToolActivity, ToolCallId,
        ToolEffect, TurnId, UiEvent,
    };

    use super::ui_channel;

    fn pending_wake_bytes(receiver: &super::UiEventReceiver) -> usize {
        let mut buffer = [0_u8; 64];
        (&receiver.wake).read(&mut buffer).unwrap_or(0)
    }

    #[test]
    fn a_burst_of_events_writes_one_wake_byte_until_the_ui_drains() {
        let (sender, receiver) = ui_channel().unwrap();
        for _ in 0..100 {
            sender.send(UiEvent::HelpRequested);
        }
        assert_eq!(pending_wake_bytes(&receiver), 1);
        receiver.drain_wake();
        assert_eq!(receiver.events.try_iter().count(), 100);
        sender.send(UiEvent::HelpRequested);
        assert_eq!(pending_wake_bytes(&receiver), 1);
    }

    #[test]
    fn a_file_approval_is_reviewed_by_the_sender_and_reaches_the_ui_without_its_copy() {
        let (sender, receiver) = ui_channel().unwrap();
        sender.send(UiEvent::ApprovalRequested {
            turn_id: TurnId::new(1),
            request: Box::new(ApprovalRequest {
                id: RequestId::new(1),
                tool_name: "edit_file".to_owned(),
                call_id: ToolCallId::new("call-1"),
                description: CallDescription {
                    title: "Editing notes.md".to_owned(),
                    label: None,
                    activity: ToolActivity::Edit,
                    effect: ToolEffect::Irreversible,
                    concurrency: Concurrency::Serial,
                },
                tool_arguments_preview: String::new(),
                tool_arguments_truncated: false,
                scope: ApprovalScope {
                    target: None,
                    access: PathAccess::WorkspaceOnly,
                    always: None,
                },
                command: None,
                file: Some(FileMutation {
                    target: PathBuf::from("/workspace/notes.md"),
                    state: FileMutationState::Changes,
                }),
                change: Some(ProposedFileChange {
                    display_path: "notes.md".to_owned(),
                    before: Some(Arc::from(&b"old\n"[..])),
                    after: Arc::from(&b"new\n"[..]),
                }),
                origin: ApprovalOrigin::ActiveSession,
            }),
        });
        sender.send(UiEvent::HelpRequested);
        let deliveries: Vec<_> = receiver.events.try_iter().collect();
        assert!(deliveries[0].file.is_some());
        assert!(matches!(
            &deliveries[0].event,
            UiEvent::ApprovalRequested { request, .. } if request.change.is_none()
        ));
        assert!(deliveries[1].file.is_none());
    }
}

#[cfg(test)]
mod failure_tests {
    use ofx_contract::{TurnId, TurnOutcome, UiEvent};

    use super::super::test_shell::TestShell;

    #[test]
    fn a_failed_turn_keeps_its_error_under_its_own_prompt() {
        let mut test = TestShell::start();
        test.submit("one");
        test.submit("two");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test.deliver(UiEvent::ApiStatus {
            turn_id: TurnId::new(1),
            text: "⚠ API request failed · HTTP 400 · invalid_request_error: rejected".to_owned(),
        });
        test.deliver(UiEvent::TurnFinished {
            turn_id: TurnId::new(1),
            outcome: TurnOutcome::Failed,
        });
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(2),
        });
        test.deliver(UiEvent::AssistantText {
            turn_id: TurnId::new(2),
            text: "Second answer.".to_owned(),
        });
        test.deliver(UiEvent::TurnFinished {
            turn_id: TurnId::new(2),
            outcome: TurnOutcome::Completed,
        });
        let screen = test.screen();
        let one = screen.find("┃ one").unwrap();
        let error = screen
            .find("⚠ API request failed · HTTP 400 · invalid_request_error: rejected")
            .unwrap();
        let two = screen.find("┃ two").unwrap();
        let answer = screen.find("Second answer.").unwrap();
        assert!(one < error && error < two && two < answer, "{screen}");
    }

    #[test]
    fn status_for_a_turn_that_is_no_longer_visible_is_ignored() {
        let mut test = TestShell::start();
        test.submit("one");
        test.deliver(UiEvent::ApiStatus {
            turn_id: TurnId::new(9),
            text: "⚠ stale".to_owned(),
        });
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test.deliver(UiEvent::TurnFinished {
            turn_id: TurnId::new(1),
            outcome: TurnOutcome::Failed,
        });
        assert!(!test.screen().contains("stale"));
    }
}
