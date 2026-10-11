use ofx_contract::{UiCommand, UsageMenuUpdate, UsageScope};

use super::Shell;
use super::usage_menu::UsageMenu;
use crate::footer::compact_command_menu_presentation::{
    UsageMenuView, usage_desired_row_count, usage_menu_hint_row, usage_menu_rows,
    usage_visible_model_items,
};
use crate::input::{Action, COMPOSER_INPUT_LIMIT_BYTES, InputEvent, PasteOwner};
use crate::row_text::Row;
use crate::terminal::TerminalError;

const RESERVED_ROWS: usize = 3;

impl UsageMenu {
    fn view(&self) -> UsageMenuView<'_> {
        UsageMenuView {
            scope: self.scope(),
            selected_model: self.selected_model,
            expanded_model: self.expanded_model,
            model_window_start: self.model_window_start,
            report: self.report.as_deref(),
            refresh_failed: self.refresh_failed,
        }
    }
}

impl Shell<'_> {
    pub(super) fn update_usage_menu(&mut self, update: UsageMenuUpdate) {
        match update {
            UsageMenuUpdate::Opened { scope, report } => {
                self.skills_menu = None;
                self.help_menu = None;
                self.settings_menu = None;
                self.statusline_menu = None;
                self.workspace_menu = None;
                self.mcp_menu = None;
                self.model_menu = None;
                self.close_picker();
                self.usage_menu =
                    Some(report.map_or_else(|| UsageMenu::loading(scope), UsageMenu::opened));
            }
            UsageMenuUpdate::Report(report) => {
                if let Some(menu) = self.answering_usage_menu(report.scope) {
                    menu.replace(report);
                }
            }
            UsageMenuUpdate::Loading(scope) => {
                if let Some(menu) = self.answering_usage_menu(scope) {
                    menu.set_loading_scope(scope);
                }
            }
            UsageMenuUpdate::Failed(scope) => {
                if let Some(menu) = self.answering_usage_menu(scope) {
                    menu.record_refresh_failure(scope);
                }
            }
        }
        self.invalidate();
    }

    pub(super) fn usage_menu_band(&self) -> Option<(Vec<Row>, Row)> {
        let menu = self.usage_menu.as_ref()?;
        if self.approval.is_some() || self.question.is_some() {
            return None;
        }
        let view = menu.view();
        let rows = usage_menu_rows(
            &self.theme,
            &view,
            self.usage_visible_rows(&view),
            self.cols(),
        );
        let mut band = Vec::with_capacity(rows.len() + 2);
        band.push(Row::new());
        band.extend(rows);
        band.push(Row::new());
        Some((band, usage_menu_hint_row(&self.theme, self.cols())))
    }

    pub(super) fn handle_usage_menu_input(
        &mut self,
        event: &InputEvent,
    ) -> Result<(), TerminalError> {
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                26 => return self.suspend(),
                b'\t' => self.cycle_usage_scope(true),
                b'\r' => self.toggle_usage_model(),
                b'r' | b'R' => self.reload_usage(),
                10 => self.move_usage_model(1),
                11 => self.move_usage_model(-1),
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
                    self.usage_menu = None;
                    self.invalidate();
                }
                Action::CursorUp => self.move_usage_model(-1),
                Action::CursorDown => self.move_usage_model(1),
                Action::CursorLeft => self.step_usage_scope(UsageScope::previous),
                Action::CursorRight => self.step_usage_scope(UsageScope::next),
                Action::TogglePermissionMode => self.cycle_usage_scope(false),
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

    fn answering_usage_menu(&mut self, scope: UsageScope) -> Option<&mut UsageMenu> {
        self.usage_menu.as_mut().filter(|menu| menu.answers(scope))
    }

    fn usage_visible_rows(&self, view: &UsageMenuView<'_>) -> usize {
        usage_desired_row_count(view)
            .min(usize::from(self.layout.rows).saturating_sub(RESERVED_ROWS))
    }

    fn usage_visible_models(&self) -> usize {
        self.usage_menu.as_ref().map_or(0, |menu| {
            let view = menu.view();
            usage_visible_model_items(&view, self.usage_visible_rows(&view))
        })
    }

    fn move_usage_model(&mut self, delta: isize) {
        let visible = self.usage_visible_models();
        if let Some(menu) = &mut self.usage_menu {
            menu.move_model(delta, visible);
        }
    }

    fn toggle_usage_model(&mut self) {
        let visible = self.usage_visible_models();
        if let Some(menu) = &mut self.usage_menu {
            menu.toggle_expanded(visible);
        }
    }

    fn cycle_usage_scope(&mut self, forward: bool) {
        let Some(menu) = &self.usage_menu else {
            return;
        };
        let scope = cycled(menu.navigation_scope(), forward);
        self.request_usage(scope);
    }

    fn step_usage_scope(&mut self, step: fn(UsageScope) -> UsageScope) {
        let Some(menu) = &self.usage_menu else {
            return;
        };
        let current = menu.navigation_scope();
        let scope = step(current);
        if scope != current {
            self.request_usage(scope);
        }
    }

    fn request_usage(&mut self, scope: UsageScope) {
        if let Some(menu) = &mut self.usage_menu {
            menu.request(scope);
        }
        self.send(UiCommand::RefreshUsage { scope });
    }

    fn reload_usage(&mut self) {
        if let Some(menu) = &self.usage_menu {
            let scope = menu.navigation_scope();
            self.send(UiCommand::ReloadUsage { scope });
        }
    }
}

fn cycled(scope: UsageScope, forward: bool) -> UsageScope {
    match (scope, forward) {
        (UsageScope::Days30, true) | (UsageScope::Hours24, false) => UsageScope::Days7,
        (UsageScope::Days7, true) | (UsageScope::Session, false) => UsageScope::Hours24,
        (UsageScope::Hours24, true) | (UsageScope::Days30, false) => UsageScope::Session,
        (UsageScope::Session, true) | (UsageScope::Days7, false) => UsageScope::Days30,
    }
}

#[cfg(test)]
mod tests;
