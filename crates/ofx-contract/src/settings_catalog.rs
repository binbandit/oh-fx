use crate::types::PermissionMode;
use crate::ui::{StatuslineItem, StatuslineToggles};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SettingCategory {
    #[default]
    All,
    Interface,
    Agent,
    Notifications,
    Advanced,
}

impl SettingCategory {
    pub const ALL: [Self; 5] = [
        Self::All,
        Self::Interface,
        Self::Agent,
        Self::Notifications,
        Self::Advanced,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Interface => "Interface",
            Self::Agent => "Agent",
            Self::Notifications => "Notifications",
            Self::Advanced => "Advanced",
        }
    }

    #[must_use]
    pub fn cycled(self, delta: isize) -> Self {
        let count = Self::ALL.len();
        let index = Self::ALL
            .iter()
            .position(|category| *category == self)
            .unwrap_or(0);
        Self::ALL[(index + count).saturating_add_signed(delta) % count]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingId {
    StatuslineContext,
    StatuslineSession,
    StatuslineWorkspace,
    Model,
    FastMode,
    PermissionMode,
    SessionTitles,
    StartupScrollback,
    PromptHistory,
}

impl SettingId {
    const fn tag(self) -> &'static str {
        match self {
            Self::StatuslineContext => "statusline_context",
            Self::StatuslineSession => "statusline_session",
            Self::StatuslineWorkspace => "statusline_workspace",
            Self::Model => "model",
            Self::FastMode => "fast_mode",
            Self::PermissionMode => "permission_mode",
            Self::SessionTitles => "session_titles",
            Self::StartupScrollback => "startup_scrollback",
            Self::PromptHistory => "prompt_history",
        }
    }

    pub const fn statusline_item(self) -> Option<StatuslineItem> {
        match self {
            Self::StatuslineContext => Some(StatuslineItem::Context),
            Self::StatuslineSession => Some(StatuslineItem::Session),
            Self::StatuslineWorkspace => Some(StatuslineItem::Workspace),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastModeSetting {
    Unavailable,
    Off,
    On,
}

impl FastModeSetting {
    pub const fn new(enabled: bool, supported: bool) -> Self {
        match (enabled, supported) {
            (true, _) => Self::On,
            (false, true) => Self::Off,
            (false, false) => Self::Unavailable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsSnapshot {
    pub model: String,
    pub fast_mode: FastModeSetting,
    pub permission_mode: PermissionMode,
    pub statusline: StatuslineToggles,
    pub session_titles: bool,
    pub startup_scrollback: bool,
    pub prompt_history: bool,
}

impl SettingsSnapshot {
    pub fn value(&self, id: SettingId) -> &str {
        if let Some(item) = id.statusline_item() {
            return on_off(self.statusline.enabled(item));
        }
        match id {
            SettingId::Model => &self.model,
            SettingId::FastMode => on_off(self.fast_mode == FastModeSetting::On),
            SettingId::PermissionMode => self.permission_mode.display_label(),
            SettingId::SessionTitles => on_off(self.session_titles),
            SettingId::StartupScrollback => on_off(self.startup_scrollback),
            _ => on_off(self.prompt_history),
        }
    }

    pub fn option_count(&self, id: SettingId) -> usize {
        match id {
            SettingId::Model => 0,
            SettingId::FastMode if self.fast_mode == FastModeSetting::Unavailable => 0,
            SettingId::PermissionMode => PERMISSION_OPTIONS.len(),
            _ => ON_OFF_OPTIONS.len(),
        }
    }

    pub fn option_at(&self, id: SettingId, index: usize) -> Option<&'static str> {
        if index >= self.option_count(id) {
            return None;
        }
        let options: &[&'static str] = if id == SettingId::PermissionMode {
            &PERMISSION_OPTIONS
        } else {
            &ON_OFF_OPTIONS
        };
        options.get(index).copied()
    }

    pub fn selected_option_index(&self, id: SettingId) -> Option<usize> {
        let current = self.value(id);
        (0..self.option_count(id)).find(|index| {
            self.option_at(id, *index)
                .is_some_and(|option| option.eq_ignore_ascii_case(current))
        })
    }

    pub fn cycle_change(&self, id: SettingId, delta: isize) -> Option<SettingChange> {
        let count = self.option_count(id);
        if count == 0 || delta == 0 {
            return None;
        }
        let current = self.selected_option_index(id).unwrap_or(0);
        let next = (current + count).saturating_add_signed(delta) % count;
        self.change_at(id, next)
    }

    pub fn change_at(&self, id: SettingId, index: usize) -> Option<SettingChange> {
        Some(SettingChange {
            setting: id,
            value: self.option_at(id, index)?,
        })
    }

    pub fn filtered_count(&self, category: SettingCategory, query: &str) -> usize {
        SPECS
            .iter()
            .filter(|spec| self.matches(spec, category, query))
            .count()
    }

    pub fn item_at(
        &self,
        category: SettingCategory,
        query: &str,
        index: usize,
    ) -> Option<SettingItem<'_>> {
        SPECS
            .iter()
            .filter(|spec| self.matches(spec, category, query))
            .nth(index)
            .map(|spec| SettingItem {
                id: spec.id,
                label: spec.label,
                value: self.value(spec.id),
            })
    }

    fn matches(&self, spec: &Spec, category: SettingCategory, query: &str) -> bool {
        (category == SettingCategory::All || spec.category == category)
            && query
                .split_ascii_whitespace()
                .all(|token| self.token_matches(spec, token))
    }

    fn token_matches(&self, spec: &Spec, token: &str) -> bool {
        [
            spec.label,
            spec.description,
            self.value(spec.id),
            spec.id.tag(),
        ]
        .iter()
        .any(|text| contains_ignoring_ascii_case(text, token))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingItem<'a> {
    pub id: SettingId,
    pub label: &'static str,
    pub value: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingChange {
    pub setting: SettingId,
    pub value: &'static str,
}

impl SettingChange {
    pub fn enabled(self) -> Option<bool> {
        match self.value {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }
    }
}

struct Spec {
    id: SettingId,
    category: SettingCategory,
    label: &'static str,
    description: &'static str,
}

const SPECS: [Spec; 9] = [
    Spec {
        id: SettingId::StatuslineContext,
        category: SettingCategory::Interface,
        label: "Status line context",
        description: "Show context usage in the status line",
    },
    Spec {
        id: SettingId::StatuslineSession,
        category: SettingCategory::Interface,
        label: "Status line session",
        description: "Show the session title in the status line",
    },
    Spec {
        id: SettingId::StatuslineWorkspace,
        category: SettingCategory::Interface,
        label: "Status line workspace",
        description: "Show the workspace path and Git branch in the status line",
    },
    Spec {
        id: SettingId::Model,
        category: SettingCategory::Agent,
        label: "Model",
        description: "Choose the model used for new turns",
    },
    Spec {
        id: SettingId::FastMode,
        category: SettingCategory::Agent,
        label: "Fast mode",
        description: "Use faster inference when the model supports it",
    },
    Spec {
        id: SettingId::PermissionMode,
        category: SettingCategory::Agent,
        label: "Permission mode",
        description: "Choose when oh-fx asks before taking actions",
    },
    Spec {
        id: SettingId::SessionTitles,
        category: SettingCategory::Agent,
        label: "Session titles",
        description: "Generate a short session title from the first prompt",
    },
    Spec {
        id: SettingId::StartupScrollback,
        category: SettingCategory::Advanced,
        label: "Startup scrollback",
        description: "Restore terminal output when oh-fx starts",
    },
    Spec {
        id: SettingId::PromptHistory,
        category: SettingCategory::Advanced,
        label: "Prompt history",
        description: "Save accepted prompts and slash commands for composer history",
    },
];

const ON_OFF_OPTIONS: [&str; 2] = ["off", "on"];
const PERMISSION_OPTIONS: [&str; 3] = ["ask", "auto", "full access"];

const fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

fn contains_ignoring_ascii_case(text: &str, token: &str) -> bool {
    text.as_bytes()
        .windows(token.len())
        .any(|window| window.eq_ignore_ascii_case(token.as_bytes()))
}

#[cfg(test)]
mod tests;
