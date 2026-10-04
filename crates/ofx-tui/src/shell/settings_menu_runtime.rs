use ofx_contract::{SettingCategory, SettingId, SettingsSnapshot, UiCommand};

use super::Shell;
use super::model_menu::ModelMenu;
use crate::footer::settings_menu_presentation::{
    InlineModels, MAX_INLINE_ROWS, SettingsView, settings_menu_hint_row, settings_menu_rows,
    visible_items_for_budget, visible_model_items_for_budget,
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

    fn view<'a>(&'a self, query: &'a str, models: Option<InlineModels<'a>>) -> SettingsView<'a> {
        SettingsView {
            snapshot: &self.snapshot,
            category: self.category,
            query,
            selected: self.selected,
            window_start: self.window_start,
            models,
        }
    }

    fn selected_setting(&self, query: &str) -> Option<SettingId> {
        let count = self.snapshot.filtered_count(self.category, query);
        let index = self.selected.checked_rem(count)?;
        Some(self.snapshot.item_at(self.category, query, index)?.id)
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
            menu.view(query, self.inline_models()),
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
        let owned = self.route_settings_menu_key(event);
        if owned {
            self.gestures.disarm_ctrl_c_exit();
        }
        owned
    }

    fn route_settings_menu_key(&mut self, event: &InputEvent) -> bool {
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                b'\r' => self.submit_settings_menu_selection(),
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

    fn inline_models(&self) -> Option<InlineModels<'_>> {
        self.model_menu.as_ref().map(|menu| InlineModels {
            menu,
            catalog: &self.catalog,
        })
    }

    fn cycle_settings_category(&mut self, delta: isize) -> bool {
        if let Some(menu) = &mut self.settings_menu {
            menu.cycle_category(delta);
            self.model_menu = None;
        }
        true
    }

    fn move_settings_menu(&mut self, delta: isize) -> bool {
        let budget = self.menu_budget(MAX_INLINE_ROWS);
        let query = self.composer.text();
        let Some(menu) = &self.settings_menu else {
            return true;
        };
        let view = menu.view(query, self.inline_models());
        if self.model_menu.is_some() {
            let visible = visible_model_items_for_budget(view, budget).max(1);
            if let Some(models) = &mut self.model_menu {
                models.move_selection(self.catalog.models(), delta, visible);
            }
            return true;
        }
        let visible = visible_items_for_budget(view, budget);
        if let Some(menu) = &mut self.settings_menu {
            menu.moved(query, delta, visible);
        }
        true
    }

    fn change_selected_setting(&mut self, delta: isize) -> bool {
        if self.model_menu.is_some() {
            if delta < 0 {
                self.apply_inline_settings_model_selection();
            }
            return true;
        }
        let query = self.composer.text();
        let Some(menu) = &self.settings_menu else {
            return true;
        };
        let Some(setting) = menu.selected_setting(query) else {
            return true;
        };
        if setting == SettingId::Model {
            if delta > 0 {
                self.open_inline_settings_models();
            }
            return true;
        }
        if menu.snapshot.option_count(setting) == 0 {
            return true;
        }
        if setting == SettingId::PromptHistory {
            self.prompt_history_stepped();
        }
        self.send(UiCommand::StepSetting { setting, delta });
        true
    }

    fn submit_settings_menu_selection(&mut self) -> bool {
        if self.model_menu.is_some() {
            self.apply_inline_settings_model_selection();
            return true;
        }
        let query = self.composer.text();
        let selected = self
            .settings_menu
            .as_ref()
            .and_then(|menu| menu.selected_setting(query));
        if selected == Some(SettingId::Model) {
            self.open_inline_settings_models();
        }
        true
    }

    fn open_inline_settings_models(&mut self) {
        self.model_menu = Some(ModelMenu::default());
        self.request_catalog(true);
    }

    fn apply_inline_settings_model_selection(&mut self) {
        let Some(model) = self
            .model_menu
            .as_ref()
            .and_then(|menu| menu.selected_id(self.catalog.models()))
            .map(str::to_owned)
        else {
            return;
        };
        self.send(UiCommand::SelectModelFromSettings { model });
        self.model_menu = None;
    }
}

#[cfg(test)]
mod tests;
