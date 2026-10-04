use ofx_contract::{
    FastModeSetting, ModelCatalog, ModelOption, SettingChange, SettingId, SettingsSnapshot, UiEvent,
};

use super::{CatalogFetch, Controller, ControllerState};
use crate::app_commands::{ModelChange, Work, capabilities_of, listed};
use crate::app_session_runtime::Persistence;
use crate::session_commands::{
    handle_history, handle_settings, save_session_titles_setting, startup_scrollback_setting,
};

const STARTUP_SCROLLBACK_ON: &str = "startup-scrollback on";
const STARTUP_SCROLLBACK_OFF: &str = "startup-scrollback off";

pub(super) struct MenuSettings {
    startup_scrollback: bool,
    prompt_history: bool,
}

impl MenuSettings {
    pub(super) fn new(prompt_history: bool) -> Self {
        Self {
            startup_scrollback: true,
            prompt_history,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsUpdate {
    Opened,
    Changed,
}

impl Controller {
    pub(super) async fn open_settings_menu(&mut self) {
        if self.state.load_menu_settings() {
            self.show_settings(SettingsUpdate::Opened).await;
        }
    }

    pub(super) async fn change_setting(&mut self, change: SettingChange) {
        if self.state.wants_fast_toggle(change) {
            self.change_model(ModelChange::ToggleFast).await;
        } else {
            self.state.change_setting(change);
        }
        self.show_settings(SettingsUpdate::Changed).await;
    }

    async fn show_settings(&mut self, update: SettingsUpdate) {
        let catalog = if self.state.fast_mode {
            ModelCatalog::Failed { retry: None }
        } else {
            self.catalog.source.catalog().await
        };
        self.state.show_settings(update, listed(&catalog));
    }
}

impl CatalogFetch {
    pub(super) fn open_settings_menu(&mut self, state: &mut ControllerState) {
        if state.load_menu_settings() {
            self.show_settings(state, SettingsUpdate::Opened);
        }
    }

    pub(super) fn change_setting(
        &mut self,
        state: &mut ControllerState,
        persistence: &mut Option<Persistence>,
        change: SettingChange,
        work: Work,
    ) {
        if state.wants_fast_toggle(change) {
            self.change(state, persistence, ModelChange::ToggleFast, work);
        } else {
            state.change_setting(change);
        }
        self.show_settings(state, SettingsUpdate::Changed);
    }

    fn show_settings(&mut self, state: &ControllerState, update: SettingsUpdate) {
        let catalog = if state.fast_mode {
            Some(ModelCatalog::Failed { retry: None })
        } else {
            self.source.cached()
        };
        match catalog {
            Some(catalog) if self.waiting.is_empty() => {
                state.show_settings(update, listed(&catalog));
            }
            _ => {
                self.settings = Some(update);
                self.request();
            }
        }
    }
}

impl ControllerState {
    fn load_menu_settings(&mut self) -> bool {
        let Some(startup_scrollback) = startup_scrollback_setting(&self.settings_access()) else {
            for notice in handle_settings(&self.settings_access(), &self.session_facts(), "") {
                self.emit(UiEvent::Notice { notice });
            }
            return false;
        };
        self.menu_settings.startup_scrollback = startup_scrollback;
        true
    }

    pub(super) fn show_settings(&self, update: SettingsUpdate, models: &[ModelOption]) {
        let snapshot = self.settings_snapshot(models);
        self.emit(match update {
            SettingsUpdate::Opened => UiEvent::SettingsMenuOpened { snapshot },
            SettingsUpdate::Changed => UiEvent::SettingsChanged { snapshot },
        });
    }

    fn wants_fast_toggle(&self, change: SettingChange) -> bool {
        change.setting == SettingId::FastMode
            && change
                .enabled()
                .is_some_and(|enabled| enabled != self.fast_mode)
    }

    fn change_setting(&mut self, change: SettingChange) {
        if change.setting == SettingId::PermissionMode {
            self.permissions.handle_command(change.value);
            return;
        }
        if change.setting == SettingId::PromptHistory {
            self.change_prompt_history(change.value);
            return;
        }
        let Some(enabled) = change.enabled() else {
            return;
        };
        if let Some(item) = change.setting.statusline_item() {
            if enabled != self.statusline.enabled(item) {
                self.toggle_statusline(item.label());
            }
            return;
        }
        match change.setting {
            SettingId::SessionTitles => self.change_session_titles(enabled),
            SettingId::StartupScrollback => self.change_startup_scrollback(enabled),
            _ => {}
        }
    }

    fn settings_snapshot(&self, models: &[ModelOption]) -> SettingsSnapshot {
        let supports_fast_mode = capabilities_of(models, &self.model).supports_fast_mode;
        SettingsSnapshot {
            model: self.model.clone(),
            fast_mode: FastModeSetting::new(self.fast_mode, supports_fast_mode),
            permission_mode: self.permissions.mode(),
            statusline: self.statusline,
            session_titles: self.setup.session_titles_enabled(),
            startup_scrollback: self.menu_settings.startup_scrollback,
            prompt_history: self.menu_settings.prompt_history,
        }
    }

    fn change_session_titles(&mut self, enabled: bool) {
        let runtime_changed = enabled != self.setup.session_titles_enabled();
        self.setup.set_session_titles(enabled);
        for notice in save_session_titles_setting(&self.settings_access(), enabled, runtime_changed)
        {
            self.emit(UiEvent::Notice { notice });
        }
    }

    fn change_startup_scrollback(&mut self, enabled: bool) {
        let command = if enabled {
            STARTUP_SCROLLBACK_ON
        } else {
            STARTUP_SCROLLBACK_OFF
        };
        for notice in handle_settings(&self.settings_access(), &self.session_facts(), command) {
            self.emit(UiEvent::Notice { notice });
        }
        if let Some(saved) = startup_scrollback_setting(&self.settings_access()) {
            self.menu_settings.startup_scrollback = saved;
        }
    }

    fn change_prompt_history(&mut self, value: &str) {
        let (enabled, notices) = handle_history(&self.settings_access(), value);
        if let Some(enabled) = enabled {
            self.menu_settings.prompt_history = enabled;
            self.emit(UiEvent::PromptHistoryChanged { enabled });
        }
        for notice in notices {
            self.emit(UiEvent::Notice { notice });
        }
    }
}
