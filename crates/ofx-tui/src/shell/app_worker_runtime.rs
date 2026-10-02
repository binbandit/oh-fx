use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use ofx_contract::{
    CompactionActivity, Notice, NoticeTone, TurnId, TurnOutcome, UiCommand, UiEvent,
};

use super::{ActiveTurn, FreshScreen, Shell, SubmissionState};
use crate::output::activity_status::TurnPhase;
use crate::output::compaction_activity::CompactionStatus;
use crate::render_engine::transcript_blocks::{Entry, HelpEntry};

const PARAGRAPH_BREAK: &str = "\n\n";

#[derive(Clone)]
pub struct UiEventSender {
    events: Sender<UiEvent>,
    wake: Arc<UnixStream>,
    woken: Arc<AtomicBool>,
}

impl UiEventSender {
    pub fn send(&self, event: UiEvent) {
        if self.events.send(event).is_ok() && !self.woken.swap(true, Ordering::AcqRel) {
            let _ = (&*self.wake).write(&[1]);
        }
    }
}

pub struct UiEventReceiver {
    events: Receiver<UiEvent>,
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

impl Shell<'_> {
    pub(super) fn drain_ui_events(&mut self) {
        self.events.drain_wake();
        loop {
            match self.events.events.try_recv() {
                Ok(event) => self.handle_ui_event(event),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.should_exit = true;
                    return;
                }
            }
        }
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        self.mark_dirty();
        match event {
            UiEvent::TurnStarted { turn_id } => self.turn_started(turn_id),
            UiEvent::AssistantText { turn_id, text } => {
                if self.is_visible_turn(turn_id) {
                    self.assistant_text(&text);
                }
            }
            UiEvent::Operational { turn_id, text } => {
                if self.is_visible_turn(turn_id) {
                    self.operational_text(&text);
                }
            }
            UiEvent::ReasoningText { turn_id, text } => {
                if let Some(turn) = self.visible_turn(turn_id) {
                    turn.tokens.consume_reasoning(&text);
                }
            }
            UiEvent::ToolStarted { turn_id, .. } => {
                self.end_assistant_step(turn_id);
                if let Some(turn) = self.visible_turn(turn_id) {
                    turn.phase = TurnPhase::Running;
                }
            }
            UiEvent::ApprovalRequested { turn_id, request } => {
                self.end_assistant_step(turn_id);
                self.approval_requested(turn_id, *request);
            }
            UiEvent::ToolRejected { turn_id, .. } => self.end_assistant_step(turn_id),
            UiEvent::ToolFinished { .. }
            | UiEvent::ContextNotice { .. }
            | UiEvent::Recovery { .. } => {}
            UiEvent::UsageReported { turn_id, usage } => {
                if let Some(turn) = self.visible_turn(turn_id) {
                    turn.tokens.settle(usage.output_tokens);
                }
            }
            UiEvent::TurnFinished { turn_id, outcome } => self.turn_finished(turn_id, outcome),
            UiEvent::ApiStatus { turn_id, text } => {
                if let Some(turn) = self.visible_turn(turn_id) {
                    turn.failure = Some(text);
                }
            }
            UiEvent::Notice { notice } => self.push_entry(Entry::Notice(notice)),
            UiEvent::ModelSelected { model } => self.options.model = model,
            UiEvent::PermissionModeChanged {
                mode,
                full_access_warning,
            } => self.permission_mode_changed(mode, full_access_warning),
            UiEvent::HelpRequested => self.help_requested(),
            UiEvent::StatsRequested => self.stats_requested(),
            UiEvent::CompactionActivity { activity } => self.compaction_activity(activity),
            UiEvent::ConversationCleared { first_kept_prompt } => {
                self.conversation_cleared(first_kept_prompt);
            }
            UiEvent::ExitRequested => self.should_exit = true,
        }
    }

    fn help_requested(&mut self) {
        let mut specs: Vec<_> = self.options.commands.iter().collect();
        specs.sort_by_key(|spec| spec.category);
        let commands = specs
            .into_iter()
            .map(|spec| HelpEntry {
                command: spec.command.clone(),
                description: spec.description.clone(),
            })
            .collect();
        self.push_entry(Entry::HelpCatalog {
            categories: self.options.command_categories.clone(),
            commands,
        });
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
                self.compaction = Some(CompactionStatus::preparing(now_ms));
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
            CompactionActivity::Ended(end) => {
                self.compaction = Some(CompactionStatus::ended(end, now_ms));
                self.promote_next();
            }
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

    pub(super) fn expire_compaction_feedback(&mut self, now_ms: i64) {
        if self.compaction.is_some_and(|status| status.expired(now_ms)) {
            self.compaction = None;
            self.mark_dirty();
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
        self.dismiss_approval();
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
        let Some(index) = self
            .outstanding
            .iter()
            .position(|submission| submission.turn_id.is_none())
        else {
            return;
        };
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
            SubmissionState::Queued => {}
        }
    }

    fn assistant_text(&mut self, text: &str) {
        let Some(turn) = &mut self.turn else {
            return;
        };
        turn.phase = TurnPhase::Generating;
        turn.tokens.consume_content(text);
        let trailing = text.len() - text.trim_end_matches('\n').len();
        turn.step_break = if trailing == text.len() {
            turn.step_break
                .map(|missing| missing.saturating_sub(trailing))
        } else {
            Some(PARAGRAPH_BREAK.len().saturating_sub(trailing))
        };
        let mut events = Vec::new();
        turn.markdown.push(text, &mut events);
        self.transcript.append_assistant(events, &self.theme);
    }

    fn end_assistant_step(&mut self, turn_id: TurnId) {
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| turn.turn_id == Some(turn_id))
        else {
            return;
        };
        let Some(missing) = turn.step_break.take() else {
            return;
        };
        let mut events = Vec::new();
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
        let was_visible = self
            .outstanding
            .remove(index)
            .is_some_and(|submission| submission.state == SubmissionState::Active);
        if was_visible && let Some(turn) = self.turn.take() {
            self.dismiss_approval();
            self.finish_visible_turn(turn, outcome);
        }
        self.promote_next();
    }

    fn finish_visible_turn(&mut self, mut turn: ActiveTurn, outcome: TurnOutcome) {
        if outcome != TurnOutcome::Interrupted {
            let mut events = Vec::new();
            turn.markdown.flush(&mut events);
            self.transcript.append_assistant(events, &self.theme);
        }
        match outcome {
            TurnOutcome::Completed => {
                let duration_ms = u64::try_from(self.now_ms() - turn.started_ms).unwrap_or(0);
                self.push_entry(Entry::TurnSummary {
                    duration_ms,
                    progress: turn.tokens.progress(),
                });
            }
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
        self.turn = Some(turn);
        self.push_entry(Entry::UserTurn { text });
    }

    pub(super) fn cancel_visible_turn(&mut self) {
        if self.turn.take().is_none() {
            return;
        }
        self.dismiss_approval();
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
        self.push_entry(Entry::Cancellation);
        self.promote_next();
    }
}

#[cfg(test)]
mod tests {
    use ofx_contract::{
        CallDescription, CompactionActivity, CompactionEnd, Concurrency, ToolActivity, ToolCallId,
        ToolEffect, TurnId, TurnOutcome, UiCommand, UiEvent,
    };

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
    fn help_lists_commands_grouped_by_category_under_the_category_tabs() {
        let mut test = TestShell::start();
        test.submit("/help");
        assert_eq!(
            test.sent(),
            [UiCommand::RunCommand {
                text: "/help".to_owned()
            }]
        );
        test.deliver(UiEvent::HelpRequested);
        let screen = test.screen();
        assert!(
            screen.contains(
                "Commands 4  [All]  General  Model\n\n  /help     \n  /clear    \n  /quit     \n  /model"
            ),
            "{screen}"
        );
    }

    fn compaction(activity: CompactionActivity) -> UiEvent {
        UiEvent::CompactionActivity { activity }
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
                    prompt: "after".to_owned()
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

    fn tool_started(turn: u64) -> UiEvent {
        UiEvent::ToolStarted {
            turn_id: TurnId::new(turn),
            call_id: ToolCallId::new("call-1"),
            tool_name: "read_file".to_owned(),
            description: CallDescription {
                title: "Reading README.md".to_owned(),
                activity: ToolActivity::Read,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Parallel,
            },
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
            test.deliver(tool_started(1));
            test.deliver(tool_started(1));
            test.deliver(text(1, after));
            test.deliver(finished(1, TurnOutcome::Completed));
            let screen = test.screen();
            assert!(
                screen.contains("  I will read it.\n\n  It describes a service.\n"),
                "{before:?}\n{screen}"
            );
        }
    }

    #[test]
    fn a_tool_step_without_text_before_it_adds_no_blank_rows() {
        let mut test = TestShell::start();
        test.submit("one");
        test.deliver(started(1));
        test.deliver(tool_started(1));
        test.deliver(tool_started(1));
        test.deliver(text(1, "It describes a service."));
        test.deliver(finished(1, TurnOutcome::Completed));
        let screen = test.screen();
        assert!(
            screen.contains("┃ one\n\n  It describes a service.\n"),
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
                    prompt: "one".to_owned()
                },
                UiCommand::RunCommand {
                    text: "/clear".to_owned()
                },
                UiCommand::Submit {
                    prompt: "two".to_owned()
                },
            ]
        );
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

    use ofx_contract::UiEvent;

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
