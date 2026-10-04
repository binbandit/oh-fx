use std::sync::Arc;

use ofx_config::{
    ProfilePaths, SettingsWriteFailure, save_permission_mode, save_yolo_acknowledged,
};
use ofx_contract::{
    LivePermissionMode, Notice, NoticeTone, PermissionGate, PermissionMode, UiEvent,
};
use ofx_permissions::PermissionPolicy;

use crate::app_agent_runtime::Emit;
use crate::user_settings::{self, Unsaved, unsaved_notice};

const PERMISSIONS_TOPIC: &str = "permissions";
const MODE_PREFERENCE_TOPIC: &str = "permission-mode";
const ACKNOWLEDGMENT_TOPIC: &str = "full-access-acknowledgment";
const PERMISSIONS_USAGE: &str = "usage: /permissions [ask|auto|full-access|reset]\n       /permissions remember <allow|deny> <tool-name> <arguments-json>\n       /permissions revoke <rule-id>";
const RESET_NOTICE: &str = "permissions reset to ask, session grants cleared";
const SAVED_SESSION_REQUIRED: &str =
    "saved-session permission rules require an active saved session";
const NO_SAVED_SESSION_RULES: &str = "saved-session permission rules: none";
const SHELL_TOPIC: &str = "shell";
const SHELL_USAGE: &str = "usage: /shell reload";
const SHELL_RELOADED: &str =
    "The next command reloads your shell startup files. Remembered command approvals were reset.";

pub(crate) struct PermissionRuntime {
    mode: LivePermissionMode,
    policy: Arc<PermissionPolicy>,
    preferences: Option<ProfilePaths>,
    yolo_acknowledged: bool,
    yolo_acknowledgment_attempted: bool,
    emit: Emit,
}

impl PermissionRuntime {
    pub(crate) fn new(
        mode: LivePermissionMode,
        policy: Arc<PermissionPolicy>,
        preferences: Option<ProfilePaths>,
        yolo_acknowledged: bool,
        emit: Emit,
    ) -> Self {
        Self {
            mode,
            policy,
            preferences,
            yolo_acknowledged,
            yolo_acknowledgment_attempted: false,
            emit,
        }
    }

    pub(crate) fn mode(&self) -> PermissionMode {
        self.mode.get()
    }

    pub(crate) fn toggle_mode(&self) {
        self.select_mode(match self.mode.get() {
            PermissionMode::Ask => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Yolo,
            PermissionMode::Yolo => PermissionMode::Ask,
        });
    }

    pub(crate) fn handle_command(&self, rest: &str) {
        if rest.is_empty() {
            self.notice(
                NoticeTone::Neutral,
                PERMISSIONS_TOPIC,
                &format!("{}\n{PERMISSIONS_USAGE}", self.policy.notice_body()),
            );
            self.notice(
                NoticeTone::Neutral,
                PERMISSIONS_TOPIC,
                NO_SAVED_SESSION_RULES,
            );
            return;
        }
        if manages_saved_session_rules(rest) {
            self.notice(NoticeTone::Error, PERMISSIONS_TOPIC, SAVED_SESSION_REQUIRED);
            return;
        }
        if let Some(mode) = PermissionMode::parse(rest) {
            self.select_mode(mode);
            let tone = if mode == PermissionMode::Yolo {
                NoticeTone::Warning
            } else {
                NoticeTone::Neutral
            };
            self.notice(
                tone,
                PERMISSIONS_TOPIC,
                &format!("mode set to {}", mode.display_label()),
            );
            return;
        }
        if rest.eq_ignore_ascii_case("reset") {
            self.reset();
            self.notice(NoticeTone::Neutral, PERMISSIONS_TOPIC, RESET_NOTICE);
            return;
        }
        self.notice(NoticeTone::Error, "", PERMISSIONS_USAGE);
    }

    pub(crate) fn reload_shell(&self, rest: &str) {
        if rest.trim_matches([' ', '\t']) != "reload" {
            self.notice(NoticeTone::Error, "", SHELL_USAGE);
            return;
        }
        self.policy.forget_command_approvals();
        self.notice(NoticeTone::Neutral, SHELL_TOPIC, SHELL_RELOADED);
    }

    pub(crate) fn full_access_warning_shown(&mut self) {
        if self.yolo_acknowledged || self.yolo_acknowledgment_attempted {
            return;
        }
        self.yolo_acknowledgment_attempted = true;
        match self.save(save_yolo_acknowledged) {
            Ok(()) => self.yolo_acknowledged = true,
            Err(failure) => self.report_unsaved(ACKNOWLEDGMENT_TOPIC, &failure),
        }
    }

    fn select_mode(&self, mode: PermissionMode) {
        self.set_mode(mode);
        self.persist_mode();
    }

    fn reset(&self) {
        self.set_mode(PermissionMode::Ask);
        self.policy.forget_approvals();
        self.persist_mode();
    }

    fn set_mode(&self, mode: PermissionMode) {
        self.mode.set(mode);
        (self.emit)(UiEvent::PermissionModeChanged {
            mode,
            full_access_warning: mode == PermissionMode::Yolo && !self.yolo_acknowledged,
        });
    }

    fn persist_mode(&self) {
        let mode = self.mode.get();
        if let Err(failure) = self.save(|paths| save_permission_mode(paths, mode)) {
            self.report_unsaved(MODE_PREFERENCE_TOPIC, &failure);
        }
    }

    fn save(
        &self,
        commit: impl FnOnce(&ProfilePaths) -> Result<(), SettingsWriteFailure>,
    ) -> Result<(), Unsaved> {
        user_settings::save(self.preferences.as_ref(), commit)
    }

    fn report_unsaved(&self, topic: &str, unsaved: &Unsaved) {
        (self.emit)(UiEvent::Notice {
            notice: unsaved_notice(topic, unsaved),
        });
    }

    fn notice(&self, tone: NoticeTone, topic: &str, body: &str) {
        (self.emit)(UiEvent::Notice {
            notice: Notice::new(tone, topic, body),
        });
    }
}

fn manages_saved_session_rules(rest: &str) -> bool {
    let word = rest.split([' ', '\t']).next().unwrap_or_default();
    word.eq_ignore_ascii_case("remember") || word.eq_ignore_ascii_case("revoke")
}

#[cfg(test)]
mod tests;
