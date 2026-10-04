use ofx_contract::{Notice, NoticeTone};

use super::{MAX_PROMPT_HISTORY, Shell};
use crate::composer::Composer;
use crate::render_engine::transcript_blocks::Entry;

type PromptSaver = Box<dyn FnMut(&str) -> Result<(), String>>;

pub struct PromptHistory {
    entries: Vec<String>,
    saver: Option<PromptSaver>,
    recording: bool,
}

impl PromptHistory {
    pub fn disabled() -> Self {
        Self {
            entries: Vec::new(),
            saver: None,
            recording: false,
        }
    }

    pub fn enabled(
        entries: Vec<String>,
        saver: impl FnMut(&str) -> Result<(), String> + 'static,
    ) -> Self {
        Self {
            entries,
            saver: Some(Box::new(saver)),
            recording: true,
        }
    }

    pub fn paused(saver: impl FnMut(&str) -> Result<(), String> + 'static) -> Self {
        Self {
            entries: Vec::new(),
            saver: Some(Box::new(saver)),
            recording: false,
        }
    }

    pub(super) fn take(&mut self) -> Self {
        std::mem::replace(self, Self::disabled())
    }
}

pub(super) struct HistoryRecorder {
    saver: Option<PromptSaver>,
    recording: bool,
    unanswered_steps: usize,
    warning_active: bool,
}

impl HistoryRecorder {
    pub(super) fn install(history: PromptHistory, composer: &mut Composer) -> Self {
        composer.install_history(history.entries);
        Self {
            saver: history.saver,
            recording: history.recording,
            unanswered_steps: 0,
            warning_active: false,
        }
    }

    fn records(&self) -> bool {
        self.recording && self.saver.is_some()
    }
}

impl Shell<'_> {
    pub(super) fn prompt_history_changed(&mut self, enabled: bool) {
        let history = &mut self.history;
        history.unanswered_steps = history.unanswered_steps.saturating_sub(1);
        history.recording = enabled && history.unanswered_steps == 0;
    }

    pub(super) fn prompt_history_stepped(&mut self) {
        self.history.unanswered_steps += 1;
        self.history.recording = false;
    }

    pub(super) fn record_prompt_history(&mut self) {
        if self.history.records() {
            self.composer.record_history(MAX_PROMPT_HISTORY);
        }
    }

    pub(super) fn record_command_history(&mut self, command: &str) {
        if !self.history.records() {
            return;
        }
        self.composer
            .record_text_history(MAX_PROMPT_HISTORY, command);
        self.save_accepted_input(command);
    }

    pub(super) fn save_accepted_input(&mut self, text: &str) {
        if !self.history.recording {
            return;
        }
        let Some(saver) = &mut self.history.saver else {
            return;
        };
        match saver(text) {
            Ok(()) => self.history.warning_active = false,
            Err(_) if self.history.warning_active => {}
            Err(error) => {
                self.history.warning_active = true;
                self.push_entry(Entry::Notice(Notice::new(
                    NoticeTone::Warning,
                    "history",
                    format!("failed to save input history ({error}); submission continued"),
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use ofx_contract::{UiCommand, UiEvent};

    use super::*;
    use crate::shell::test_shell::TestShell;

    const UP: &[u8] = b"\x1b[A";
    const DOWN: &[u8] = b"\x1b[B";

    fn press(test: &mut TestShell, bytes: &[u8]) -> String {
        test.type_bytes(bytes);
        test.draining(|shell| assert!(shell.step().unwrap().is_none()));
        test.shell.composer.text().to_owned()
    }

    fn recording(
        entries: &[&str],
        outcomes: &[Result<(), &str>],
    ) -> (PromptHistory, Rc<RefCell<Vec<String>>>) {
        let saved = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&saved);
        let mut outcomes: VecDeque<Result<(), String>> = outcomes
            .iter()
            .map(|outcome| outcome.map_err(str::to_owned))
            .collect();
        let history = PromptHistory::enabled(
            entries.iter().map(|entry| (*entry).to_owned()).collect(),
            move |text| {
                sink.borrow_mut().push(text.to_owned());
                outcomes.pop_front().unwrap_or(Ok(()))
            },
        );
        (history, saved)
    }

    #[test]
    fn installed_history_is_recalled_newest_first_and_returns_to_the_draft() {
        let (history, _) = recording(&["older", "newer"], &[]);
        let mut test = TestShell::start_with(|options| options.prompt_history = history);
        assert_eq!(press(&mut test, b"draft"), "draft");
        assert_eq!(press(&mut test, UP), "draft");
        assert_eq!(test.shell.composer.cursor(), 0);
        assert_eq!(press(&mut test, UP), "newer");
        assert_eq!(press(&mut test, UP), "newer");
        assert_eq!(press(&mut test, UP), "older");
        assert_eq!(press(&mut test, UP), "older");
        assert_eq!(press(&mut test, UP), "older");
        assert!(test.screen().contains("┃ older"));
        assert_eq!(press(&mut test, DOWN), "older");
        assert_eq!(press(&mut test, DOWN), "newer");
        assert_eq!(press(&mut test, DOWN), "draft");
    }

    #[test]
    fn accepted_prompts_and_resolved_commands_are_saved_and_recalled() {
        let (history, saved) = recording(&["from an earlier session"], &[]);
        let mut test = TestShell::start_with(|options| options.prompt_history = history);
        test.submit("  spaced prompt \n");
        test.submit("/he");
        test.submit("  ");
        assert_eq!(*saved.borrow(), ["  spaced prompt \n", "/help"]);
        assert_eq!(
            test.sent(),
            [
                UiCommand::Submit {
                    prompt: "spaced prompt".to_owned(),
                    skills: Vec::new(),
                },
                UiCommand::RunCommand {
                    text: "/help".to_owned()
                },
            ]
        );
        assert_eq!(press(&mut test, UP), "/help");
        press(&mut test, UP);
        assert_eq!(press(&mut test, UP), "  spaced prompt \n");
        press(&mut test, UP);
        press(&mut test, UP);
        assert_eq!(press(&mut test, UP), "from an earlier session");
    }

    #[test]
    fn save_failures_warn_once_until_a_save_succeeds() {
        let (history, saved) = recording(
            &[],
            &[
                Err("PromptHistoryLockBusy"),
                Err("PromptHistoryLockBusy"),
                Ok(()),
                Err("PromptHistoryWriteFailed"),
            ],
        );
        let mut test = TestShell::start_with(|options| options.prompt_history = history);
        for prompt in ["one", "two", "three", "four"] {
            test.submit(prompt);
        }
        assert_eq!(saved.borrow().len(), 4);
        let screen = test.screen();
        assert_eq!(
            screen
                .matches("failed to save input history (PromptHistoryLockBusy); submission")
                .count(),
            1,
            "{screen}"
        );
        assert_eq!(
            screen
                .matches("failed to save input history (PromptHistoryWriteFailed); submission")
                .count(),
            1,
            "{screen}"
        );
    }

    #[test]
    fn history_switched_off_stops_saving_and_switched_on_resumes() {
        let (history, saved) = recording(&["older"], &[]);
        let mut test = TestShell::start_with(|options| options.prompt_history = history);
        test.deliver(UiEvent::PromptHistoryChanged { enabled: false });
        test.submit("private");
        test.submit("/help");
        assert!(saved.borrow().is_empty());
        assert_eq!(press(&mut test, UP), "older");
        press(&mut test, DOWN);
        test.deliver(UiEvent::PromptHistoryChanged { enabled: true });
        test.submit("kept");
        assert_eq!(*saved.borrow(), ["kept"]);
        assert_eq!(press(&mut test, UP), "kept");
    }

    #[test]
    fn history_that_starts_paused_saves_once_switched_on() {
        let saved = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&saved);
        let history = PromptHistory::paused(move |text| {
            sink.borrow_mut().push(text.to_owned());
            Ok(())
        });
        let mut test = TestShell::start_with(|options| options.prompt_history = history);
        test.submit("before");
        assert!(saved.borrow().is_empty());
        test.deliver(UiEvent::PromptHistoryChanged { enabled: true });
        test.submit("after");
        assert_eq!(*saved.borrow(), ["after"]);
        assert_eq!(press(&mut test, UP), "after");
        assert_eq!(press(&mut test, UP), "after");
    }

    #[test]
    fn disabled_history_neither_saves_nor_recalls() {
        let mut test =
            TestShell::start_with(|options| options.prompt_history = PromptHistory::disabled());
        test.submit("one");
        test.submit("/help");
        assert_eq!(press(&mut test, UP), "");
    }
}
