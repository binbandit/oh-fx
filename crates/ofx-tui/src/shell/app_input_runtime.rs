use ofx_contract::{Notice, NoticeTone, UiCommand};

use super::upgrade_shortcut::requests_upgrade;
use super::{FreshScreen, Shell};
use crate::composer::{
    DeletionKind, HistoryNavigation, InsertResult, KillKind, VerticalDirection, VerticalOutcome,
};
use crate::input::gesture_state::PressResult;
use crate::input::{
    Action, COMPOSER_INPUT_LIMIT_BYTES, DecodedTerminalAction, InputContext, InputEvent, MoveKind,
    PasteOutcome, PasteOwner, RawTerminalInput, ShortcutAction, TextOwner,
};
use crate::render_engine::transcript_blocks::Entry;
use crate::terminal::TAGGED_CURSOR_QUERY;

const LIMIT_REJECTED: &str = "That edit exceeds the local prompt safety limit and was not applied.";
const PASTE_TRAILING_INPUT: &str =
    "Paste was not applied because extra input followed its end marker.";
const PASTE_UNSUPPORTED_BYTES: &str =
    "Pasted text contains unsupported bytes and was not added to the draft.";

impl Shell<'_> {
    pub(super) fn process_input(&mut self) -> Result<(), crate::terminal::TerminalError> {
        loop {
            if self.should_exit {
                return Ok(());
            }
            let context = InputContext {
                now_ms: self.now_ms(),
                cancel_pending: self.working(),
                text_owner: TextOwner::Composer,
                native_clear_row: self.native_clear_row(),
            };
            let Some(event) = self.input.next_event(context) else {
                return Ok(());
            };
            self.handle_input_event(event)?;
        }
    }

    pub(super) fn flush_pending_input(&mut self) -> Result<(), crate::terminal::TerminalError> {
        let now_ms = self.now_ms();
        self.input.poll_theme_monitor(now_ms);
        self.input.poll_native_clear_probe(now_ms);
        self.process_input()?;
        if let Some(event) = self.input.flush_escape(now_ms) {
            self.handle_input_event(event)?;
            self.process_input()?;
        }
        if let Some(event) = self.input.settle_delivery_epoch(now_ms) {
            self.handle_input_event(event)?;
        }
        self.process_input()
    }

    fn handle_input_event(
        &mut self,
        event: InputEvent,
    ) -> Result<(), crate::terminal::TerminalError> {
        match event {
            InputEvent::NativeClearProbe => {
                match self
                    .terminal
                    .write_all_unless_full(TAGGED_CURSOR_QUERY.as_bytes())
                {
                    Ok(true) => {}
                    Ok(false) => self.input.cancel_native_clear_probe(false),
                    Err(_) => self.input.cancel_native_clear_probe(true),
                }
                return Ok(());
            }
            InputEvent::NativeClearDetected => {
                if !self.resized_since_frame() {
                    self.start_fresh_transcript(FreshScreen::Erase);
                }
                return Ok(());
            }
            _ => {}
        }
        self.invalidate();
        if self.route_full_transcript_input(&event)? {
            return Ok(());
        }
        if requests_upgrade(&event) {
            self.apply_ready_upgrade();
            return Ok(());
        }
        if self.approval.is_some() {
            return self.handle_approval_input(&event);
        }
        if self.question.is_some() {
            return self.handle_question_input(event);
        }
        if self.statusline_menu.is_some() {
            return self.handle_statusline_menu_input(&event);
        }
        if self.workspace_menu.is_some() {
            return self.handle_workspace_menu_input(&event);
        }
        if self.settings_menu_owns(&event) {
            return Ok(());
        }
        let revision = self.composer.edit_revision();
        let preserved = self.model_edit_preserved(&event);
        match event {
            InputEvent::Raw(raw) => self.handle_raw(raw)?,
            InputEvent::Text(character) => {
                self.gestures.disarm_ctrl_c_exit();
                self.gestures.disarm_escape_clear();
                self.gestures.disarm_escape_interrupt();
                self.insert(character.encode_utf8(&mut [0; 4]));
            }
            InputEvent::Action(decoded) => self.handle_action(decoded),
            InputEvent::Paste(outcome) => self.handle_paste(outcome),
            InputEvent::TextDropped(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => {}
        }
        let edited = self.composer.edit_revision() != revision;
        if edited {
            self.file_picker_after_edit();
            self.model_column_after_edit(preserved);
            self.help_menu_edited();
            self.provider_column_after_edit();
        }
        self.sync_settings_menu(edited);
        self.sync_skills_menu();
        self.sync_picker_query();
        self.sync_model_menu(edited);
        self.settle_model_draft();
        self.ensure_catalog();
        Ok(())
    }

    fn handle_raw(&mut self, raw: RawTerminalInput) -> Result<(), crate::terminal::TerminalError> {
        let byte = raw.byte;
        if byte != 3 && byte != 0x1b {
            self.gestures.disarm_ctrl_c_exit();
        }
        if byte != 0x1b {
            self.gestures.disarm_escape_clear();
            self.gestures.disarm_escape_interrupt();
        }
        if let Some(delta) = picker_control_delta(byte)
            && !self.full_transcript_open()
            && self.move_footer_menu(delta)
        {
            return Ok(());
        }
        match byte {
            26 => return self.suspend(),
            3 => self.handle_ctrl_c(),
            4 => self.handle_ctrl_d(),
            b'\r' if self.picker_active() => self.submit_picker_selection(),
            b'\r' => self.handle_enter(),
            b'\t' if self.picker_active() => {}
            b'\t' => self.handle_tab(),
            b' ' if self.model_query().is_some() && self.composer.selection().is_none() => {
                self.advance_model_column_on_space();
            }
            7 | 22 | 24 => {}
            _ => {
                if let Some(action) = raw.composer_shortcut {
                    self.route_shortcut(action);
                } else if byte == b'$' {
                    self.insert_dollar();
                } else if (32..127).contains(&byte) {
                    if byte == b' ' {
                        self.close_skill_mention();
                    }
                    self.insert(char::from(byte).encode_utf8(&mut [0; 4]));
                }
            }
        }
        Ok(())
    }

    fn handle_action(&mut self, decoded: DecodedTerminalAction) {
        match decoded.action {
            Action::RemappedByte(byte) => {
                self.input.replay_byte(byte);
                return;
            }
            Action::PasteStart => {
                self.input
                    .begin_paste(PasteOwner::Composer, COMPOSER_INPUT_LIMIT_BYTES);
                return;
            }
            Action::PasteEnd | Action::Ignore => return,
            Action::OpenAllSessions => {
                self.open_all_sessions();
                return;
            }
            _ => {}
        }
        self.gestures.disarm_ctrl_c_exit();
        if self.picker_active() {
            match decoded.action {
                Action::Escape => {
                    self.gestures.disarm_escape_clear();
                    self.close_picker();
                    return;
                }
                Action::TogglePermissionMode => {
                    self.toggle_picker_scope();
                    return;
                }
                _ => {}
            }
        }
        if decoded.action == Action::Escape {
            self.resolve_escape(decoded.cancel_pending);
            return;
        }
        self.gestures.disarm_escape_interrupt();
        self.gestures.disarm_escape_clear();
        if decoded.action == Action::TogglePermissionMode {
            if !self.cycle_help_menu_category(-1)
                && !self.cycle_model_menu_vendor(-1)
                && !self.cycle_skills_menu_source(-1)
            {
                self.send(UiCommand::TogglePermissionMode);
            }
            return;
        }
        if decoded.action == Action::OpenModelCatalog {
            self.toggle_model_shortcut();
            return;
        }
        if let Some(action) = decoded.composer_shortcut {
            self.route_shortcut(action);
        }
    }

    pub(super) fn handle_paste(&mut self, outcome: PasteOutcome) {
        match outcome {
            PasteOutcome::Text {
                owner: PasteOwner::Composer,
                text,
            } => {
                let start = self
                    .composer
                    .selection()
                    .map_or(self.composer.cursor(), |selection| selection.start);
                match self
                    .composer
                    .insert_paste(&text, COMPOSER_INPUT_LIMIT_BYTES)
                {
                    InsertResult::LimitExceeded => self.report_limit(),
                    InsertResult::Inserted => self.open_pasted_skill_mention(start, &text),
                    InsertResult::Inactive => {}
                }
            }
            PasteOutcome::LimitExceeded { .. } => self.report_limit(),
            PasteOutcome::UnsupportedBytes { .. } => self.input_notice(PASTE_UNSUPPORTED_BYTES),
            PasteOutcome::Text { .. } | PasteOutcome::Secret { .. } | PasteOutcome::Discarded => {}
            PasteOutcome::TrailingInput { .. } => self.input_notice(PASTE_TRAILING_INPUT),
        }
    }

    pub(super) fn input_notice(&mut self, body: &str) {
        self.push_entry(Entry::Notice(Notice::new(NoticeTone::Error, "input", body)));
    }

    pub(super) fn report_limit(&mut self) {
        if self.composer.note_limit_rejection(TextOwner::Composer) {
            self.input_notice(LIMIT_REJECTED);
        }
    }

    pub(super) fn insert(&mut self, text: &str) {
        if self.composer.insert_text(text, COMPOSER_INPUT_LIMIT_BYTES)
            == InsertResult::LimitExceeded
        {
            self.report_limit();
        }
    }

    fn resolve_escape(&mut self, cancel_pending: bool) {
        if self.cancel_help_menu()
            || self.cancel_model_menu()
            || self.cancel_skills_menu()
            || self.dismiss_model_column()
            || self.dismiss_provider_column()
            || self.dismiss_file_picker()
        {
            self.gestures.disarm_escape_clear();
            self.gestures.disarm_escape_interrupt();
            return;
        }
        let now_ms = self.now_ms();
        if self.dismiss_compaction_feedback() {
            self.gestures.disarm_escape_clear();
            self.gestures.disarm_escape_interrupt();
            return;
        }
        if cancel_pending && self.pause_connectivity_wait() {
            self.gestures.disarm_escape_clear();
            self.gestures.disarm_escape_interrupt();
            return;
        }
        if cancel_pending && self.working() {
            if self.gestures.press_escape_interrupt(now_ms) == PressResult::Activated {
                self.interrupt();
            }
            self.gestures.disarm_escape_clear();
            return;
        }
        if self.composer.is_empty() {
            self.gestures.disarm_escape_clear();
            return;
        }
        if self.gestures.press_escape_clear(now_ms) == PressResult::Activated {
            self.composer.clear();
        }
    }

    fn handle_ctrl_c(&mut self) {
        if self.exit_model_shortcut() {
            return;
        }
        if self.turn.is_some() && !self.composer.is_empty() {
            self.composer.clear();
            return;
        }
        let now_ms = self.now_ms();
        if self.gestures.press_ctrl_c_exit(now_ms) == PressResult::Activated {
            self.should_exit = true;
            return;
        }
        if self.working() {
            self.interrupt();
            return;
        }
        if self.drop_held_prompt() {
            return;
        }
        self.composer.clear();
    }

    fn interrupt(&mut self) {
        if self.manual_compaction_running() {
            self.cancel_compaction();
        } else {
            self.cancel_visible_turn();
        }
    }

    fn handle_ctrl_d(&mut self) {
        if self.exit_model_shortcut() {
            return;
        }
        if !self.composer.is_empty() {
            self.composer.delete(DeletionKind::CharacterRight);
            return;
        }
        if self.turn.is_none() {
            self.should_exit = true;
        }
    }

    fn route_shortcut(&mut self, action: ShortcutAction) {
        let limit = COMPOSER_INPUT_LIMIT_BYTES;
        match action {
            ShortcutAction::Move(intent)
                if self.picker_active()
                    && matches!(intent.kind, MoveKind::VisualUp | MoveKind::VisualDown) =>
            {
                self.move_picker(if intent.kind == MoveKind::VisualUp {
                    -1
                } else {
                    1
                });
            }
            ShortcutAction::Move(intent) => match intent.kind {
                MoveKind::VisualUp
                | MoveKind::VisualDown
                | MoveKind::PageUp
                | MoveKind::PageDown => {
                    self.move_vertical(intent.kind, intent.extend_selection);
                }
                MoveKind::CharacterLeft
                    if !intent.extend_selection
                        && self.composer.selection().is_none()
                        && self.step_back_model_column() => {}
                MoveKind::CharacterRight
                    if !intent.extend_selection && self.choose_provider_at_end() => {}
                _ => {
                    self.composer.move_cursor(intent);
                }
            },
            ShortcutAction::SelectAll => {
                self.composer.select_all();
            }
            ShortcutAction::Undo => {
                self.composer.undo();
            }
            ShortcutAction::Redo => {
                self.composer.redo();
            }
            ShortcutAction::HistoryNext => {
                if !self.move_footer_menu(1) {
                    self.navigate_history(1);
                }
            }
            ShortcutAction::DeleteBackward => {
                self.composer.delete(DeletionKind::CharacterLeft);
            }
            ShortcutAction::DeleteForward => {
                self.composer.delete(DeletionKind::CharacterRight);
            }
            ShortcutAction::DeleteWordLeft => {
                self.composer.delete(DeletionKind::WordLeft);
            }
            ShortcutAction::DeleteWordRight => {
                self.composer.delete(DeletionKind::WordRight);
            }
            ShortcutAction::DeleteWhitespaceWordLeft => {
                self.composer.kill(KillKind::WhitespaceWordLeft);
            }
            ShortcutAction::DeleteToLineStart => {
                self.composer.kill(KillKind::LineStart);
            }
            ShortcutAction::DeleteToLineEnd => {
                self.composer.kill(KillKind::LineEnd);
            }
            ShortcutAction::Yank => {
                self.composer.yank(limit);
            }
            ShortcutAction::Redraw => self.start_fresh_transcript(FreshScreen::Erase),
            ShortcutAction::InsertNewline => {
                if !self.command_skills_menu_open() && self.model_menu.is_none() {
                    self.insert("\n");
                }
            }
            ShortcutAction::CopySelection => self.copy_selection(),
            ShortcutAction::CutSelection => self.cut_selection(),
        }
    }

    fn move_vertical(&mut self, kind: MoveKind, extend_selection: bool) {
        let (direction, delta): (VerticalDirection, i32) = match kind {
            MoveKind::VisualUp | MoveKind::PageUp => (VerticalDirection::Up, -1),
            _ => (VerticalDirection::Down, 1),
        };
        let page_rows = matches!(kind, MoveKind::PageUp | MoveKind::PageDown).then(|| {
            crate::footer::input_presentation::input_row_limit(usize::from(
                self.layout.content_bottom,
            ))
        });
        let menu_delta = page_rows.map_or(delta, |rows| {
            delta.saturating_mul(i32::try_from(rows).unwrap_or(i32::MAX))
        });
        if self.move_footer_menu(menu_delta) {
            self.composer.reset_vertical();
            return;
        }
        let outcome =
            self.composer
                .move_vertical(direction, extend_selection, page_rows, self.layout.cols);
        if outcome == VerticalOutcome::ReachedTop && self.retract_waiting_steer() {
            return;
        }
        if matches!(
            outcome,
            VerticalOutcome::ReachedTop | VerticalOutcome::ReachedBottom
        ) {
            self.navigate_history(delta);
        }
    }

    fn handle_enter(&mut self) {
        if self.submit_help_menu_selection()
            || self.submit_model_menu()
            || self.submit_skills_menu_selection()
        {
            return;
        }
        if let Some(result) = self.submit_file_picker_on_enter() {
            if result == InsertResult::LimitExceeded {
                self.report_limit();
            }
            return;
        }
        if self.bare_model_command() {
            self.open_model_menu();
            return;
        }
        if self.submit_provider_column()
            || self.submit_model_column()
            || self.submit_explicit_model()
            || self.model_draft.is_some()
            || self.composer.replace_backslash_before_cursor_with_newline()
        {
            return;
        }
        self.submit();
    }

    fn handle_tab(&mut self) {
        if self.cycle_help_menu_category(1)
            || self.cycle_model_menu_vendor(1)
            || self.cycle_skills_menu_source(1)
        {
            return;
        }
        if self.bare_model_command() {
            self.open_current_model_column();
        } else if self.has_file_query() {
            if self.autocomplete_file_picker() == InsertResult::LimitExceeded {
                self.report_limit();
            }
        } else if !self.autocomplete_provider_column() {
            self.autocomplete_model_column();
        }
    }

    fn move_footer_menu(&mut self, delta: i32) -> bool {
        let rows = isize::try_from(delta).unwrap_or_default();
        if self.picker_active() {
            self.move_picker(rows);
            return true;
        }
        self.move_help_menu(rows)
            || self.move_model_menu(rows)
            || self.move_skills_menu(rows)
            || self.navigate_file_picker(delta)
            || self.navigate_model_column(delta)
            || self.navigate_provider_column(delta)
            || (self.turn.is_some() && self.bare_model_command())
    }

    fn navigate_history(&mut self, delta: i32) {
        match self
            .composer
            .navigate_history(delta, COMPOSER_INPUT_LIMIT_BYTES)
        {
            HistoryNavigation::LimitExceeded(_) => self.report_limit(),
            HistoryNavigation::Moved => self.reset_file_picker_episode(),
            HistoryNavigation::Unchanged => {}
        }
    }
}

fn picker_control_delta(byte: u8) -> Option<i32> {
    match byte {
        10 => Some(1),
        11 => Some(-1),
        _ => None,
    }
}
