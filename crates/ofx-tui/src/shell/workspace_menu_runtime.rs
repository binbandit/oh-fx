use std::os::unix::ffi::OsStrExt;

use ofx_contract::WorkspaceMenu;

use super::Shell;
use super::workspace_menu::{WorkspaceAction, WorkspaceMenuState};
use crate::footer::compact_command_menu_presentation::{
    workspace_desired_row_count, workspace_menu_hint_row, workspace_menu_rows,
};
use crate::input::{Action, COMPOSER_INPUT_LIMIT_BYTES, InputEvent, PasteOwner};
use crate::row_text::Row;
use crate::terminal::TerminalError;

const RESERVED_ROWS: usize = 3;
const ADD_COMMAND: &str = "/workspace add ";
const REMOVE_COMMAND: &str = "/workspace remove ";
const CLEAR_COMMAND: &str = "/workspace clear";

impl Shell<'_> {
    pub(super) fn open_workspace_menu(&mut self, menu: WorkspaceMenu) {
        self.skills_menu = None;
        self.help_menu = None;
        self.settings_menu = None;
        self.statusline_menu = None;
        self.model_menu = None;
        self.close_picker();
        self.workspace_menu = Some(WorkspaceMenuState::open(menu));
        self.invalidate();
    }

    pub(super) fn workspace_menu_band(&self) -> Option<(Vec<Row>, Row)> {
        let menu = self.workspace_menu.as_ref()?;
        if self.approval.is_some() || self.question.is_some() {
            return None;
        }
        let visible_rows = workspace_desired_row_count(&menu.menu)
            .min(usize::from(self.layout.rows).saturating_sub(RESERVED_ROWS));
        let rows = workspace_menu_rows(
            &self.theme,
            &menu.menu,
            menu.selected_row(),
            visible_rows,
            self.cols(),
        );
        let mut band = Vec::with_capacity(rows.len() + 2);
        band.push(Row::new());
        band.extend(rows);
        band.push(Row::new());
        Some((band, workspace_menu_hint_row(&self.theme, self.cols())))
    }

    pub(super) fn handle_workspace_menu_input(
        &mut self,
        event: &InputEvent,
    ) -> Result<(), TerminalError> {
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                26 => return self.suspend(),
                b'\r' => self.prepare_workspace_command(),
                10 => self.move_workspace_menu(1),
                11 => self.move_workspace_menu(-1),
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
                    self.workspace_menu = None;
                    self.invalidate();
                }
                Action::CursorUp => self.move_workspace_menu(-1),
                Action::CursorDown => self.move_workspace_menu(1),
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

    fn move_workspace_menu(&mut self, delta: isize) {
        if let Some(menu) = &mut self.workspace_menu {
            menu.moved(delta);
        }
    }

    fn prepare_workspace_command(&mut self) {
        let Some(menu) = &self.workspace_menu else {
            return;
        };
        let command = match menu.selected_action() {
            WorkspaceAction::Add => ADD_COMMAND.to_owned(),
            WorkspaceAction::Remove(index) => {
                let path = menu.menu.entries[index].path.as_os_str().as_bytes();
                let Ok(path) = std::str::from_utf8(path) else {
                    return;
                };
                format!("{REMOVE_COMMAND}{path}")
            }
            WorkspaceAction::Clear => CLEAR_COMMAND.to_owned(),
        };
        if command.len() > COMPOSER_INPUT_LIMIT_BYTES {
            return;
        }
        self.workspace_menu = None;
        self.composer.replace_text(&command);
        self.invalidate();
    }
}

#[cfg(test)]
mod tests;
