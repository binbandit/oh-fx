use ofx_contract::{ApprovalDecision, ApprovalRequest, TurnId, UiCommand};

use super::Shell;
use crate::footer::approval_panel::{Choice, approval_panel_rows, choices};
use crate::footer::input_presentation::ComposerView;
use crate::input::{Action, COMPOSER_INPUT_LIMIT_BYTES, InputEvent, PasteOwner};
use crate::terminal::TerminalError;
use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApprovalPrompt {
    request: ApprovalRequest,
    choice: usize,
}

impl ApprovalPrompt {
    pub(super) fn view(&self, theme: &Theme, cols: u16, rows: u16) -> ComposerView {
        ComposerView {
            rows: approval_panel_rows(
                theme,
                &self.request.title,
                self.choices(),
                self.choice,
                usize::from(cols),
                rows,
            ),
            cursor: None,
        }
    }

    fn choices(&self) -> &'static [Choice] {
        choices(self.request.scope.always.is_some())
    }
}

impl Shell<'_> {
    pub(super) fn approval_requested(&mut self, turn_id: TurnId, request: ApprovalRequest) {
        if !self.is_visible_turn(turn_id) {
            self.send(UiCommand::Approval {
                request_id: request.id,
                decision: ApprovalDecision::Deny,
            });
            return;
        }
        self.approval = Some(ApprovalPrompt { request, choice: 0 });
        self.invalidate();
    }

    pub(super) fn handle_approval_input(
        &mut self,
        event: &InputEvent,
    ) -> Result<(), TerminalError> {
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                26 => return self.suspend(),
                3 => self.decide(ApprovalDecision::Deny),
                b'\r' | b'\n' => self.decide_selected(),
                b'\t' => self.move_choice(1),
                key => self.decide_by_key(key),
            },
            InputEvent::Action(decoded) => match decoded.action {
                Action::RemappedByte(byte) => self.input.replay_byte(byte),
                Action::PasteStart => self
                    .input
                    .begin_paste(PasteOwner::Composer, COMPOSER_INPUT_LIMIT_BYTES),
                Action::Escape => self.approval_escape(),
                Action::CursorUp => self.move_choice(-1),
                Action::CursorDown => self.move_choice(1),
                _ => {}
            },
            InputEvent::Text(_) | InputEvent::Paste(_) | InputEvent::TextDropped(_) => {}
        }
        Ok(())
    }

    pub(super) fn approval_escape(&mut self) {
        self.dismiss_approval();
        self.cancel_visible_turn();
    }

    pub(super) fn dismiss_approval(&mut self) {
        if self.approval.take().is_some() {
            self.invalidate();
        }
    }

    fn decide_by_key(&mut self, key: u8) {
        let Some(prompt) = &mut self.approval else {
            return;
        };
        if let Some(index) = prompt.choices().iter().position(|choice| choice.key == key) {
            prompt.choice = index;
            self.decide_selected();
        }
    }

    fn move_choice(&mut self, step: isize) {
        if let Some(prompt) = &mut self.approval {
            let count = prompt.choices().len();
            prompt.choice = (prompt.choice + count).saturating_add_signed(step) % count;
        }
    }

    fn decide_selected(&mut self) {
        if let Some(decision) = self
            .approval
            .as_ref()
            .map(|prompt| prompt.choices()[prompt.choice].decision)
        {
            self.decide(decision);
        }
    }

    fn decide(&mut self, decision: ApprovalDecision) {
        if let Some(prompt) = self.approval.take() {
            self.invalidate();
            self.send(UiCommand::Approval {
                request_id: prompt.request.id,
                decision,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ofx_contract::{
        ApprovalDecision, ApprovalRequest, ApprovalScope, PathAccess, RequestId, SessionGrant,
        TurnId, TurnOutcome, UiCommand, UiEvent,
    };

    use super::super::test_shell::TestShell;

    const PANEL: &str = "Permission needed · Choose one";

    fn request(turn: u64, id: u64) -> UiEvent {
        request_with(
            turn,
            id,
            Some(SessionGrant::ReadsUnder(PathBuf::from("/home"))),
        )
    }

    fn request_with(turn: u64, id: u64, always: Option<SessionGrant>) -> UiEvent {
        UiEvent::ApprovalRequested {
            turn_id: TurnId::new(turn),
            request: ApprovalRequest {
                id: RequestId::new(id),
                tool_name: "read_file".to_owned(),
                title: "Reading ../notes.txt".to_owned(),
                tool_arguments_preview: r#"{"path":"../notes.txt"}"#.to_owned(),
                scope: ApprovalScope {
                    target: Some(PathBuf::from("/home/notes.txt")),
                    access: PathAccess::Within(PathBuf::from("/home")),
                    always,
                },
                command: None,
                file: None,
            },
        }
    }

    fn approving() -> TestShell {
        let mut test = TestShell::start();
        test.submit("read the notes");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test.deliver(request(1, 4));
        test
    }

    fn decision(id: u64, decision: ApprovalDecision) -> UiCommand {
        UiCommand::Approval {
            request_id: RequestId::new(id),
            decision,
        }
    }

    fn press(test: &mut TestShell, bytes: &[u8]) {
        test.type_bytes(bytes);
        test.step();
    }

    #[test]
    fn a_request_replaces_the_composer_until_a_number_decides() {
        let mut test = approving();
        let screen = test.screen();
        for line in [
            PANEL,
            "Would you like to allow this action?",
            "Reading ../notes.txt",
            "❯ 1. Yes",
            "2. Yes, and don't ask again for this request",
        ] {
            assert!(screen.contains(line), "{line}\n{screen}");
        }
        press(&mut test, b"2");
        assert_eq!(
            test.sent().last(),
            Some(&decision(4, ApprovalDecision::Always))
        );
        let screen = test.screen();
        assert!(!screen.contains(PANEL), "{screen}");
    }

    #[test]
    fn arrows_and_tab_move_the_choice_and_enter_confirms_it() {
        let mut test = approving();
        press(&mut test, b"\x1b[B");
        press(&mut test, b"\t");
        assert!(test.screen().contains("❯ 3. No"));
        press(&mut test, b"\x1b[A");
        assert!(test.screen().contains("❯ 2. Yes"));
        press(&mut test, b"\r");
        assert_eq!(
            test.sent().last(),
            Some(&decision(4, ApprovalDecision::Always))
        );
    }

    #[test]
    fn ctrl_c_denies_and_typed_text_never_reaches_the_composer() {
        let mut test = approving();
        press(&mut test, b"x");
        press(&mut test, b"\x03");
        assert_eq!(
            test.sent().last(),
            Some(&decision(4, ApprovalDecision::Deny))
        );
        assert!(test.shell.composer.is_empty());
        press(&mut test, b"y");
        assert_eq!(test.shell.composer.text(), "y");
    }

    #[test]
    fn a_request_that_remembers_nothing_offers_no_always_choice() {
        let mut test = TestShell::start();
        test.submit("read the notes");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test.deliver(request_with(1, 4, None));
        let screen = test.screen();
        assert!(screen.contains("1. Yes"), "{screen}");
        assert!(screen.contains("3. No"), "{screen}");
        assert!(!screen.contains("2."), "{screen}");
        press(&mut test, b"2");
        assert!(
            test.sent()
                .iter()
                .all(|command| !matches!(command, UiCommand::Approval { .. }))
        );
        press(&mut test, b"\x1b[A");
        assert!(test.screen().contains("❯ 3. No"));
        press(&mut test, b"3");
        assert_eq!(
            test.sent().last(),
            Some(&decision(4, ApprovalDecision::Deny))
        );
    }

    #[test]
    fn escape_cancels_the_running_turn() {
        let mut test = approving();
        assert!(test.screen().contains(PANEL));
        test.shell.approval_escape();
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::Cancel {
                turn_id: TurnId::new(1)
            })
        );
        let screen = test.screen();
        assert!(!screen.contains(PANEL), "{screen}");
        assert!(screen.contains("■ Cancelled"), "{screen}");
    }

    #[test]
    fn finishing_the_turn_or_a_hidden_turn_never_leaves_a_prompt_waiting() {
        let mut test = approving();
        assert!(test.screen().contains(PANEL));
        test.deliver(UiEvent::TurnFinished {
            turn_id: TurnId::new(1),
            outcome: TurnOutcome::Interrupted,
        });
        assert!(!test.screen().contains(PANEL));
        test.deliver(request(9, 5));
        assert_eq!(
            test.sent().last(),
            Some(&decision(5, ApprovalDecision::Deny))
        );
        assert!(!test.screen().contains(PANEL));
    }
}
