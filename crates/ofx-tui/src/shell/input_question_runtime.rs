use ofx_contract::{QuestionRequest, RequestId, TurnId, UiCommand};

use super::Shell;
use super::question_prompt::{Decision, FreeformEdit, Insertion, QuestionPrompt};
use crate::footer::input_presentation::ComposerView;
use crate::footer::question_freeform_layout::Direction;
use crate::footer::question_ui::question_panel_rows;
use crate::input::gesture_state::PressResult;
use crate::input::{
    Action, COMPOSER_INPUT_LIMIT_BYTES, DECISION_INPUT_LIMIT_BYTES, DecodedTerminalAction,
    InputEvent, MoveKind, PasteOutcome, PasteOwner, RawTerminalInput, ShortcutAction,
};
use crate::render_engine::transcript_blocks::Entry;
use crate::row_text::Row;
use crate::terminal::TerminalError;
use crate::theme::Theme;

pub(super) const LIMIT_REJECTED: &str = "That edit exceeds the input limit and was not applied.";

impl QuestionPrompt {
    pub(super) fn composer_view(&self, theme: &Theme, cols: u16) -> ComposerView {
        let rows = self.view().entry.map_or_else(
            || vec![Row::new()],
            |entry| question_panel_rows(theme, entry, usize::from(cols)),
        );
        ComposerView {
            rows,
            cursor: None,
            review: None,
        }
    }
}

impl Shell<'_> {
    pub(super) fn question_requested(&mut self, turn_id: TurnId, request: QuestionRequest) {
        if !self.is_visible_turn(turn_id) {
            self.answer_question(request.id, None);
            return;
        }
        if let Some(displaced) = self.question.replace(QuestionPrompt::new(request)) {
            self.answer_question(displaced.request_id, None);
        }
        self.foreground(super::ForegroundState::Blocked, Some(b"question"));
        self.invalidate();
    }

    pub(super) fn dismiss_question(&mut self) {
        if self.question.take().is_some() {
            self.invalidate();
        }
    }

    pub(super) fn handle_question_input(&mut self, event: InputEvent) -> Result<(), TerminalError> {
        if matches!(
            event,
            InputEvent::Text(_)
                | InputEvent::Action(DecodedTerminalAction {
                    action: Action::PasteStart,
                    ..
                })
        ) || matches!(&event, InputEvent::Raw(raw) if raw.byte != 0x1b)
        {
            self.gestures.disarm_escape_clear();
        }
        self.gestures.disarm_ctrl_c_exit();
        self.gestures.disarm_escape_interrupt();
        match event {
            InputEvent::Raw(raw) => return self.question_raw(raw),
            InputEvent::Text(character) => {
                self.insert_answer_text(character.encode_utf8(&mut [0; 4]));
            }
            InputEvent::Action(decoded) => self.question_action(decoded),
            InputEvent::Paste(outcome) => self.question_paste(outcome),
            InputEvent::TextDropped(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => {}
        }
        Ok(())
    }

    fn question_raw(&mut self, raw: RawTerminalInput) -> Result<(), TerminalError> {
        let freeform = self.question_freeform_selected();
        if freeform && let Some(edit) = raw.composer_shortcut.and_then(focused_editor_edit) {
            self.edit_answer(edit);
            return Ok(());
        }
        match raw.byte {
            26 => return self.suspend(),
            3 => self.cancel_question(),
            b'\r' | b'\n' => self.decide_question(QuestionPrompt::submit),
            b'\t' => self.with_question(QuestionPrompt::next_entry),
            0x7f | 0x08 => self.with_question(|prompt| {
                prompt.backspace();
            }),
            digit @ b'1'..=b'9' if !freeform => {
                let index = usize::from(digit - b'1');
                self.decide_question(|prompt| prompt.select_ordinal(index));
            }
            byte @ 0x20..=0x7e => {
                self.insert_answer_text(char::from(byte).encode_utf8(&mut [0; 4]));
            }
            _ => {}
        }
        Ok(())
    }

    fn question_action(&mut self, decoded: DecodedTerminalAction) {
        match decoded.action {
            Action::RemappedByte(byte) => {
                self.input.replay_byte(byte);
                return;
            }
            Action::PasteStart => {
                let owner = if self.question_freeform_selected() {
                    PasteOwner::QuestionFreeform
                } else {
                    PasteOwner::DecisionPrompt
                };
                self.input.begin_paste(owner, COMPOSER_INPUT_LIMIT_BYTES);
                return;
            }
            Action::Escape => {
                self.question_escape();
                return;
            }
            _ => {}
        }
        if let Some(ShortcutAction::Move(intent)) = decoded.composer_shortcut
            && intent.extend_selection
            && matches!(intent.kind, MoveKind::VisualUp | MoveKind::VisualDown)
        {
            let step = if intent.kind == MoveKind::VisualUp {
                -1
            } else {
                1
            };
            self.with_question(|prompt| prompt.move_choice(step));
            return;
        }
        let freeform = self.question_freeform_selected();
        if freeform && let Some(edit) = decoded.composer_shortcut.and_then(freeform_edit) {
            self.edit_answer(edit);
            return;
        }
        let cols = self.cols();
        match decoded.action {
            Action::CursorUp | Action::CursorDown => {
                let (direction, step) = if decoded.action == Action::CursorUp {
                    (Direction::Up, -1)
                } else {
                    (Direction::Down, 1)
                };
                self.with_question(|prompt| {
                    if !prompt.move_freeform_vertical(direction, cols) {
                        prompt.move_choice(step);
                    }
                });
            }
            Action::CursorLeft | Action::WordLeft => {
                self.with_question(|prompt| {
                    prompt.retreat();
                });
            }
            _ => {}
        }
    }

    fn question_escape(&mut self) {
        self.gestures.disarm_escape_interrupt();
        let draft_len = self.question.as_ref().map_or(0, QuestionPrompt::draft_len);
        if draft_len == 0 {
            self.gestures.disarm_escape_clear();
            self.cancel_question();
            return;
        }
        let now_ms = self.now_ms();
        if self.gestures.press_escape_clear(now_ms) == PressResult::Activated
            && let Some(prompt) = &mut self.question
        {
            prompt.clear_draft();
        }
    }

    fn question_paste(&mut self, outcome: PasteOutcome) {
        match outcome {
            PasteOutcome::Text {
                owner: PasteOwner::QuestionFreeform,
                text,
            } => self.insert_answer_text(&text),
            PasteOutcome::LimitExceeded {
                owner: PasteOwner::QuestionFreeform,
                ..
            } => self.report_answer_limit(),
            outcome => self.handle_paste(outcome),
        }
    }

    fn question_freeform_selected(&self) -> bool {
        self.question
            .as_ref()
            .is_some_and(QuestionPrompt::freeform_selected)
    }

    fn with_question(&mut self, change: impl FnOnce(&mut QuestionPrompt)) {
        if let Some(prompt) = &mut self.question {
            change(prompt);
        }
    }

    fn edit_answer(&mut self, edit: FreeformEdit) {
        self.with_question(|prompt| {
            prompt.edit(edit);
        });
    }

    fn insert_answer_text(&mut self, text: &str) {
        let Some(prompt) = &mut self.question else {
            return;
        };
        match prompt.insert(text, DECISION_INPUT_LIMIT_BYTES) {
            Insertion::Inserted | Insertion::Inactive => {}
            Insertion::LimitExceeded => self.report_answer_limit(),
        }
    }

    fn report_answer_limit(&mut self) {
        if self
            .question
            .as_mut()
            .is_some_and(QuestionPrompt::note_limit_rejection)
        {
            self.input_notice(LIMIT_REJECTED);
        }
    }

    fn decide_question(&mut self, decide: impl FnOnce(&mut QuestionPrompt) -> Decision) {
        let Some(prompt) = &mut self.question else {
            return;
        };
        if decide(prompt) != Decision::AllDecided {
            return;
        }
        if let Some(prompt) = self.question.take() {
            self.push_entry(Entry::QuestionResolution {
                answers: prompt.resolutions(),
            });
            self.answer_question(prompt.request_id, Some(prompt.answers()));
        }
    }

    fn cancel_question(&mut self) {
        let Some(prompt) = self.question.take() else {
            return;
        };
        self.gestures.disarm_escape_clear();
        self.cancel_visible_turn_noting(Entry::QuestionCancelled);
        self.answer_question(prompt.request_id, None);
    }

    fn answer_question(&mut self, request_id: RequestId, answers: Option<Vec<String>>) {
        self.send(UiCommand::QuestionAnswered {
            request_id,
            answers,
        });
    }
}

pub(super) fn focused_editor_edit(shortcut: ShortcutAction) -> Option<FreeformEdit> {
    match shortcut {
        ShortcutAction::Move(_)
        | ShortcutAction::DeleteWhitespaceWordLeft
        | ShortcutAction::DeleteToLineStart
        | ShortcutAction::DeleteToLineEnd => freeform_edit(shortcut),
        _ => None,
    }
}

pub(super) fn freeform_edit(shortcut: ShortcutAction) -> Option<FreeformEdit> {
    Some(match shortcut {
        ShortcutAction::Move(intent) => match intent.kind {
            MoveKind::CharacterLeft => FreeformEdit::CursorLeft,
            MoveKind::CharacterRight => FreeformEdit::CursorRight,
            MoveKind::LineStart | MoveKind::DraftStart => FreeformEdit::CursorHome,
            MoveKind::LineEnd | MoveKind::DraftEnd => FreeformEdit::CursorEnd,
            MoveKind::WordLeft => FreeformEdit::CursorWordLeft,
            MoveKind::WordRight => FreeformEdit::CursorWordRight,
            _ => return None,
        },
        ShortcutAction::DeleteForward => FreeformEdit::DeleteNext,
        ShortcutAction::DeleteWordLeft => FreeformEdit::DeleteWordLeft,
        ShortcutAction::DeleteWhitespaceWordLeft => FreeformEdit::DeleteWhitespaceWordLeft,
        ShortcutAction::DeleteWordRight => FreeformEdit::DeleteWordRight,
        ShortcutAction::DeleteToLineStart => FreeformEdit::DeleteToLineStart,
        ShortcutAction::DeleteToLineEnd => FreeformEdit::DeleteToLineEnd,
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
