use ofx_contract::{
    FastModeSetting, ModelOption, NoticeTone, ReasoningEffort, SettingChange, SettingId,
    SettingsSnapshot, UiEvent,
};

use super::{CatalogFetch, Controller, ControllerState};
use crate::app_commands::{ModelChange, Outcome, Work, capabilities_of, listed};
use crate::app_session_runtime::Persistence;
use crate::session_commands::{
    handle_history, handle_settings, save_session_titles_setting, startup_scrollback_setting,
};

const STARTUP_SCROLLBACK_ON: &str = "startup-scrollback on";
const STARTUP_SCROLLBACK_OFF: &str = "startup-scrollback off";
const EFFORT_TOPIC: &str = "effort";

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

    pub(super) async fn step_setting(&mut self, setting: SettingId, delta: isize) {
        match setting_change(setting, delta) {
            Some(change) => self.change_model(change).await,
            None => self.state.step_setting(setting, delta),
        }
        self.show_settings(SettingsUpdate::Changed).await;
    }

    pub(super) async fn select_model_from_settings(&mut self, model: String) {
        self.change_model(ModelChange::FromSettings(model)).await;
        self.show_settings(SettingsUpdate::Changed).await;
    }

    async fn show_settings(&mut self, update: SettingsUpdate) {
        let catalog = self.catalog.source.catalog().await;
        self.state.show_settings(update, listed(&catalog));
    }
}

impl CatalogFetch {
    pub(super) fn open_settings_menu(&mut self, state: &mut ControllerState) {
        if state.load_menu_settings() {
            self.show_settings(state, SettingsUpdate::Opened);
        }
    }

    pub(super) fn step_setting(
        &mut self,
        state: &mut ControllerState,
        persistence: &mut Option<Persistence>,
        setting: SettingId,
        delta: isize,
        work: Work,
    ) {
        match setting_change(setting, delta) {
            Some(change) => self.change(state, persistence, change, work),
            None => state.step_setting(setting, delta),
        }
        self.show_settings(state, SettingsUpdate::Changed);
    }

    pub(super) fn select_model_from_settings(
        &mut self,
        state: &mut ControllerState,
        persistence: &mut Option<Persistence>,
        model: String,
        work: Work,
    ) {
        self.change(state, persistence, ModelChange::FromSettings(model), work);
        self.show_settings(state, SettingsUpdate::Changed);
    }

    fn show_settings(&mut self, state: &ControllerState, update: SettingsUpdate) {
        match self.source.cached() {
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

    fn step_setting(&mut self, setting: SettingId, delta: isize) {
        if let Some(change) = self.settings_snapshot(&[]).cycle_change(setting, delta) {
            self.change_setting(&change);
        }
    }

    pub(crate) fn step_effort(&mut self, delta: isize, models: &[ModelOption]) -> Outcome {
        let snapshot = self.settings_snapshot(models);
        let Some(effort) = snapshot
            .cycle_change(SettingId::Effort, delta)
            .and_then(|change| ReasoningEffort::parse(&change.value))
        else {
            return Outcome::Unchanged;
        };
        let offered = match &effort {
            ReasoningEffort::Auto => true,
            ReasoningEffort::Named(name) => snapshot.reasoning_efforts.contains(name),
        };
        if !offered {
            let body = format!(
                "{} is not available for {}",
                effort.display_label(),
                self.model
            );
            self.notice(NoticeTone::Neutral, EFFORT_TOPIC, &body);
            return Outcome::Unchanged;
        }
        effort.clone_into(&mut self.effort);
        self.save_model_preference(EFFORT_TOPIC, Some(&effort));
        Outcome::Changed {
            effort: Some(effort),
        }
    }

    fn change_setting(&mut self, change: &SettingChange) {
        if change.setting == SettingId::PermissionMode {
            self.permissions.handle_command(&change.value);
            return;
        }
        if change.setting == SettingId::PromptHistory {
            self.change_prompt_history(&change.value);
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
            effort: self.effort.display_label().to_owned(),
            reasoning_efforts: capabilities_of(models, &self.model).reasoning_efforts,
            fast_mode: FastModeSetting::new(self.fast_mode(), supports_fast_mode),
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
        let switching_off = value.eq_ignore_ascii_case("off");
        if switching_off {
            self.menu_settings.prompt_history = false;
            self.emit(UiEvent::PromptHistoryChanged { enabled: false });
        }
        let (enabled, notices) = handle_history(&self.settings_access(), value);
        if let Some(enabled) = enabled.filter(|_| !switching_off) {
            self.menu_settings.prompt_history = enabled;
            self.emit(UiEvent::PromptHistoryChanged { enabled });
        }
        for notice in notices {
            self.emit(UiEvent::Notice { notice });
        }
    }
}

fn setting_change(setting: SettingId, delta: isize) -> Option<ModelChange> {
    match setting {
        SettingId::FastMode => Some(ModelChange::ToggleFast),
        SettingId::Effort => Some(ModelChange::StepEffort(delta)),
        _ => None,
    }
}
