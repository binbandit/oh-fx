use ofx_contract::{SettingCategory, SettingsSnapshot, UiCommand};

use super::Shell;
use crate::footer::settings_menu_presentation::{
    MAX_INLINE_ROWS, SettingsView, settings_menu_hint_row, settings_menu_rows,
    visible_items_for_budget,
};
use crate::input::{Action, InputEvent};
use crate::row_text::Row;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SettingsMenu {
    snapshot: SettingsSnapshot,
    category: SettingCategory,
    selected: usize,
    window_start: usize,
}

impl SettingsMenu {
    fn new(snapshot: SettingsSnapshot) -> Self {
        Self {
            snapshot,
            category: SettingCategory::All,
            selected: 0,
            window_start: 0,
        }
    }

    fn view<'a>(&'a self, query: &'a str) -> SettingsView<'a> {
        SettingsView {
            snapshot: &self.snapshot,
            category: self.category,
            query,
            selected: self.selected,
            window_start: self.window_start,
        }
    }

    fn reset_for_query(&mut self) {
        self.selected = 0;
        self.window_start = 0;
    }

    fn cycle_category(&mut self, delta: isize) {
        self.category = self.category.cycled(delta);
        self.reset_for_query();
    }

    fn moved(&mut self, query: &str, delta: isize, visible_items: usize) {
        let count = self.snapshot.filtered_count(self.category, query);
        if count == 0 {
            return;
        }
        let current = self.selected % count;
        let next = current.checked_add_signed(delta).unwrap_or(count - 1);
        self.selected = if next >= count { 0 } else { next };
        let width = visible_items.max(1);
        self.window_start = if count <= width {
            0
        } else if self.selected < self.window_start {
            self.selected
        } else if self.selected >= self.window_start + width {
            self.selected + 1 - width
        } else {
            self.window_start.min(count - width)
        };
    }
}

impl Shell<'_> {
    pub(super) fn open_settings_menu(&mut self, snapshot: SettingsSnapshot) {
        self.skills_menu = None;
        self.help_menu = None;
        self.statusline_menu = None;
        self.close_picker();
        self.settings_menu = Some(SettingsMenu::new(snapshot));
        self.invalidate();
    }

    pub(super) fn settings_changed(&mut self, snapshot: SettingsSnapshot) {
        if let Some(menu) = &mut self.settings_menu {
            menu.snapshot = snapshot;
            self.invalidate();
        }
    }

    pub(super) fn settings_menu_band(&self) -> Option<(Vec<Row>, Row)> {
        let menu = self.settings_menu.as_ref()?;
        if self.approval.is_some() || self.question.is_some() {
            return None;
        }
        let query = self.composer.text();
        let rows = settings_menu_rows(
            &self.theme,
            menu.view(query),
            self.menu_budget(MAX_INLINE_ROWS),
            self.cols(),
        );
        let mut band = Vec::with_capacity(rows.len() + 2);
        band.push(Row::new());
        band.extend(rows);
        band.push(Row::new());
        let hint =
            settings_menu_hint_row(&self.theme, self.cols(), self.gestures.ctrl_c_exit_armed());
        Some((band, hint))
    }

    pub(super) fn settings_menu_owns(&mut self, event: &InputEvent) -> bool {
        if self.settings_menu.is_none() {
            return false;
        }
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                b'\r' => true,
                b'\t' => self.cycle_settings_category(1),
                10 => self.move_settings_menu(1),
                11 => self.move_settings_menu(-1),
                _ => false,
            },
            InputEvent::Action(decoded) => match decoded.action {
                Action::Escape => {
                    self.gestures.disarm_escape_clear();
                    self.gestures.disarm_escape_interrupt();
                    self.settings_menu = None;
                    self.composer.clear();
                    true
                }
                Action::TogglePermissionMode => self.cycle_settings_category(-1),
                Action::CursorUp => self.move_settings_menu(-1),
                Action::CursorDown => self.move_settings_menu(1),
                Action::CursorLeft => self.change_selected_setting(-1),
                Action::CursorRight => self.change_selected_setting(1),
                _ => false,
            },
            InputEvent::Text(_)
            | InputEvent::Paste(_)
            | InputEvent::TextDropped(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => false,
        }
    }

    pub(super) fn sync_settings_menu(&mut self, edited: bool) {
        if edited && let Some(menu) = &mut self.settings_menu {
            menu.reset_for_query();
        }
    }

    fn cycle_settings_category(&mut self, delta: isize) -> bool {
        if let Some(menu) = &mut self.settings_menu {
            menu.cycle_category(delta);
        }
        true
    }

    fn move_settings_menu(&mut self, delta: isize) -> bool {
        let budget = self.menu_budget(MAX_INLINE_ROWS);
        let query = self.composer.text();
        if let Some(menu) = &mut self.settings_menu {
            let visible = visible_items_for_budget(menu.view(query), budget);
            menu.moved(query, delta, visible);
        }
        true
    }

    fn change_selected_setting(&mut self, delta: isize) -> bool {
        let query = self.composer.text();
        let setting = self.settings_menu.as_ref().and_then(|menu| {
            let count = menu.snapshot.filtered_count(menu.category, query);
            let index = menu.selected.checked_rem(count)?;
            let item = menu.snapshot.item_at(menu.category, query, index)?;
            (menu.snapshot.option_count(item.id) > 0).then_some(item.id)
        });
        if let Some(setting) = setting {
            self.send(UiCommand::StepSetting { setting, delta });
        }
        true
    }
}

#[cfg(test)]
mod tests;
