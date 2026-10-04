use ofx_contract::UiCommand;

use super::Shell;
use crate::footer::compact_command_menu_presentation::{
    STATUSLINE_CHOICES, desired_row_count, statusline_menu_hint_row, statusline_menu_rows,
};
use crate::input::{Action, COMPOSER_INPUT_LIMIT_BYTES, InputEvent, PasteOwner};
use crate::row_text::Row;
use crate::terminal::TerminalError;

const RESERVED_ROWS: usize = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct StatuslineMenu {
    selected: usize,
}

impl StatuslineMenu {
    fn moved(self, delta: isize) -> Self {
        let count = STATUSLINE_CHOICES.len();
        Self {
            selected: (self.selected + count).saturating_add_signed(delta) % count,
        }
    }
}

impl Shell<'_> {
    pub(super) fn open_statusline_menu(&mut self) {
        self.skills_menu = None;
        self.help_menu = None;
        self.settings_menu = None;
        self.close_picker();
        self.statusline_menu = Some(StatuslineMenu::default());
        self.invalidate();
    }

    pub(super) fn statusline_menu_band(&self) -> Option<(Vec<Row>, Row)> {
        let menu = self.statusline_menu?;
        if self.approval.is_some() || self.question.is_some() {
            return None;
        }
        let visible_rows =
            desired_row_count().min(usize::from(self.layout.rows).saturating_sub(RESERVED_ROWS));
        let rows = statusline_menu_rows(
            &self.theme,
            self.statusline.toggles(),
            menu.selected,
            visible_rows,
            self.cols(),
        );
        let mut band = Vec::with_capacity(rows.len() + 2);
        band.push(Row::new());
        band.extend(rows);
        band.push(Row::new());
        Some((band, statusline_menu_hint_row(&self.theme, self.cols())))
    }

    pub(super) fn handle_statusline_menu_input(
        &mut self,
        event: &InputEvent,
    ) -> Result<(), TerminalError> {
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                26 => return self.suspend(),
                b'\r' => self.toggle_statusline_segment(),
                10 => self.move_statusline_menu(1),
                11 => self.move_statusline_menu(-1),
                _ => {}
            },
            InputEvent::Action(decoded) => match decoded.action {
                Action::RemappedByte(byte) => self.input.replay_byte(byte),
                Action::PasteStart => self
                    .input
                    .begin_paste(PasteOwner::Composer, COMPOSER_INPUT_LIMIT_BYTES),
                Action::Escape => {
                    self.gestures.disarm_escape_clear();
                    self.gestures.disarm_escape_interrupt();
                    self.statusline_menu = None;
                    self.invalidate();
                }
                Action::CursorUp => self.move_statusline_menu(-1),
                Action::CursorDown => self.move_statusline_menu(1),
                Action::CursorLeft | Action::CursorRight => self.toggle_statusline_segment(),
                _ => {}
            },
            InputEvent::Paste(outcome) => self.handle_paste(outcome.clone()),
            InputEvent::Text(_)
            | InputEvent::TextDropped(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => {}
        }
        Ok(())
    }

    fn move_statusline_menu(&mut self, delta: isize) {
        if let Some(menu) = &mut self.statusline_menu {
            *menu = menu.moved(delta);
        }
    }

    fn toggle_statusline_segment(&mut self) {
        let Some(menu) = self.statusline_menu else {
            return;
        };
        let (item, _) = STATUSLINE_CHOICES[menu.selected];
        let enabled = !self.statusline.toggles().enabled(item);
        self.statusline.set(item, enabled);
        self.send(UiCommand::ToggleStatusline { item });
    }
}

#[cfg(test)]
mod tests;
