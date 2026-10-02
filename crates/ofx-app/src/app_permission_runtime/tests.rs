use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use ofx_contract::SessionGrant;
use tempfile::TempDir;

use super::*;

const USAGE_ERROR: &str = "error||usage: /permissions [ask|auto|full-access|reset]\n       /permissions remember <allow|deny> <tool-name> <arguments-json>\n       /permissions revoke <rule-id>";

struct Fixture {
    _home: TempDir,
    paths: ProfilePaths,
    mode: LivePermissionMode,
    policy: Arc<PermissionPolicy>,
    events: Arc<Mutex<Vec<UiEvent>>>,
}

impl Fixture {
    fn new(mode: PermissionMode) -> Self {
        let home = tempfile::tempdir().unwrap();
        let paths = ProfilePaths {
            config: home.path().join("config"),
            data: home.path().join("data"),
            state: home.path().join("state"),
            cache: home.path().join("cache"),
        };
        let mode = LivePermissionMode::from(mode);
        Self {
            policy: Arc::new(PermissionPolicy::new(mode.clone(), "/workspace")),
            _home: home,
            paths,
            mode,
            events: Arc::default(),
        }
    }

    fn with_settings(mode: PermissionMode, settings: &str) -> Self {
        let fixture = Self::new(mode);
        fs::create_dir_all(&fixture.paths.config).unwrap();
        fs::write(fixture.settings_file(), settings).unwrap();
        fixture
    }

    fn runtime(&self, acknowledged: bool) -> PermissionRuntime {
        self.runtime_saving_to(Some(self.paths.clone()), acknowledged)
    }

    fn runtime_saving_to(
        &self,
        preferences: Option<ProfilePaths>,
        acknowledged: bool,
    ) -> PermissionRuntime {
        let events = Arc::clone(&self.events);
        PermissionRuntime::new(
            self.mode.clone(),
            Arc::clone(&self.policy),
            preferences,
            acknowledged,
            Arc::new(move |event| events.lock().unwrap().push(event)),
        )
    }

    fn settings_file(&self) -> PathBuf {
        self.paths.config.join("settings.json")
    }

    fn saved(&self) -> Option<String> {
        fs::read_to_string(self.settings_file()).ok()
    }

    fn take(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .drain(..)
            .map(|event| match event {
                UiEvent::PermissionModeChanged {
                    mode,
                    full_access_warning,
                } => format!("mode {} warn={full_access_warning}", mode.label()),
                UiEvent::Notice { notice } => {
                    let tone = match notice.tone {
                        NoticeTone::Neutral => "neutral",
                        NoticeTone::Warning => "warning",
                        NoticeTone::Error => "error",
                        other => unreachable!("{other:?}"),
                    };
                    format!("{tone}|{}|{}", notice.topic, notice.body)
                }
                other => format!("{other:?}"),
            })
            .collect()
    }
}

fn saved_mode(label: &str) -> String {
    format!("{{\"permission_mode\":\"{label}\"}}\n")
}

#[test]
fn toggling_cycles_ask_auto_and_full_access_and_saves_each_mode() {
    let fixture = Fixture::new(PermissionMode::Ask);
    let runtime = fixture.runtime(false);
    for (mode, label, warn) in [
        (PermissionMode::Auto, "auto", false),
        (PermissionMode::Yolo, "yolo", true),
        (PermissionMode::Ask, "ask", false),
    ] {
        runtime.toggle_mode();
        assert_eq!(fixture.mode.get(), mode);
        assert_eq!(fixture.take(), [format!("mode {label} warn={warn}")]);
        assert_eq!(fixture.saved(), Some(saved_mode(label)));
    }
}

#[test]
fn an_acknowledged_full_access_switch_shows_no_warning() {
    let fixture = Fixture::new(PermissionMode::Auto);
    fixture.runtime(true).toggle_mode();
    assert_eq!(fixture.take(), ["mode yolo warn=false"]);
}

#[test]
fn the_command_sets_each_mode_and_names_it_with_upstreams_tones() {
    let fixture = Fixture::new(PermissionMode::Auto);
    let runtime = fixture.runtime(false);
    for (argument, mode, label, notice) in [
        (
            "Full Access",
            PermissionMode::Yolo,
            "yolo",
            "warning|permissions|mode set to full access",
        ),
        (
            "ask",
            PermissionMode::Ask,
            "ask",
            "neutral|permissions|mode set to ask",
        ),
        (
            "AUTO",
            PermissionMode::Auto,
            "auto",
            "neutral|permissions|mode set to auto",
        ),
        (
            "full-access",
            PermissionMode::Yolo,
            "yolo",
            "warning|permissions|mode set to full access",
        ),
        (
            "yolo",
            PermissionMode::Yolo,
            "yolo",
            "warning|permissions|mode set to full access",
        ),
    ] {
        runtime.handle_command(argument);
        assert_eq!(fixture.mode.get(), mode, "{argument}");
        let warn = mode == PermissionMode::Yolo;
        assert_eq!(
            fixture.take(),
            [format!("mode {label} warn={warn}"), notice.to_owned()],
            "{argument}"
        );
        assert_eq!(fixture.saved(), Some(saved_mode(label)), "{argument}");
    }
}

#[test]
fn reset_returns_to_ask_and_forgets_every_session_grant() {
    let fixture = Fixture::new(PermissionMode::Yolo);
    fixture
        .policy
        .remember_approval(&SessionGrant::ReadsUnder(PathBuf::from("/elsewhere")));
    let runtime = fixture.runtime(false);
    runtime.handle_command("RESET");
    assert_eq!(fixture.mode.get(), PermissionMode::Ask);
    assert_eq!(
        fixture.take(),
        [
            "mode ask warn=false",
            "neutral|permissions|permissions reset to ask, session grants cleared",
        ]
    );
    assert_eq!(fixture.saved(), Some(saved_mode("ask")));
    assert!(
        fixture
            .policy
            .notice_body()
            .ends_with("session grants: (none)")
    );
}

#[test]
fn the_bare_command_reports_the_live_status_without_changing_or_saving_anything() {
    let fixture = Fixture::new(PermissionMode::Auto);
    fixture
        .policy
        .remember_approval(&SessionGrant::GrepsUnder(PathBuf::from("/elsewhere")));
    fixture.runtime(false).handle_command("");
    assert_eq!(
        fixture.take(),
        [
            format!(
                "neutral|permissions|mode=auto\nconfigured rules: (none)\nsession grants:\n - grep -> ../elsewhere/**\n{PERMISSIONS_USAGE}"
            ),
            "neutral|permissions|saved-session permission rules: none".to_owned(),
        ]
    );
    assert_eq!(fixture.mode.get(), PermissionMode::Auto);
    assert_eq!(fixture.saved(), None);
}

#[test]
fn unknown_arguments_report_usage_and_saved_session_rules_need_a_saved_session() {
    let fixture = Fixture::new(PermissionMode::Auto);
    let runtime = fixture.runtime(false);
    for argument in ["add", "remove", "auto now", "full", "reset all"] {
        runtime.handle_command(argument);
        assert_eq!(fixture.take(), [USAGE_ERROR], "{argument}");
    }
    for argument in ["remember", "REVOKE 3", "remember\tallow read_file {}"] {
        runtime.handle_command(argument);
        assert_eq!(
            fixture.take(),
            ["error|permissions|saved-session permission rules require an active saved session"],
            "{argument}"
        );
    }
    assert_eq!(fixture.mode.get(), PermissionMode::Auto);
    assert_eq!(fixture.saved(), None);
}

#[test]
fn a_mode_that_cannot_be_saved_still_applies_and_says_so() {
    let fixture = Fixture::with_settings(PermissionMode::Ask, r#"{"workspaces":"legacy"}"#);
    let runtime = fixture.runtime(false);
    runtime.handle_command("auto");
    assert_eq!(fixture.mode.get(), PermissionMode::Auto);
    assert_eq!(
        fixture.take(),
        [
            "mode auto warn=false",
            "error|permission-mode|active for this process but not saved to user settings (InvalidSettingsFormat)",
            "neutral|permissions|mode set to auto",
        ]
    );
    assert_eq!(
        fixture.saved().as_deref(),
        Some(r#"{"workspaces":"legacy"}"#)
    );
    let homeless = fixture.runtime_saving_to(None, false);
    homeless.toggle_mode();
    assert_eq!(fixture.mode.get(), PermissionMode::Yolo);
    assert_eq!(
        fixture.take(),
        [
            "mode yolo warn=true",
            "error|permission-mode|active for this process but not saved to user settings (HomeNotSet)",
        ]
    );
}

#[test]
fn the_full_access_acknowledgment_is_saved_once_and_silences_later_warnings() {
    let fixture = Fixture::with_settings(PermissionMode::Ask, "{\"permission_mode\":\"ask\"}");
    let mut runtime = fixture.runtime(false);
    runtime.toggle_mode();
    runtime.toggle_mode();
    assert_eq!(
        fixture.take(),
        ["mode auto warn=false", "mode yolo warn=true"]
    );
    runtime.full_access_warning_shown();
    assert_eq!(
        fixture.saved().as_deref(),
        Some("{\"permission_mode\":\"yolo\",\"yolo_acknowledged\":true}\n")
    );
    fs::write(fixture.settings_file(), "{}").unwrap();
    runtime.full_access_warning_shown();
    assert_eq!(fixture.saved().as_deref(), Some("{}"));
    runtime.handle_command("yolo");
    assert_eq!(
        fixture.take(),
        [
            "mode yolo warn=false",
            "warning|permissions|mode set to full access",
        ]
    );
}

#[test]
fn a_failed_acknowledgment_is_reported_and_attempted_once_per_process() {
    let fixture = Fixture::with_settings(PermissionMode::Yolo, r#"{"workspaces":"legacy"}"#);
    let mut runtime = fixture.runtime(false);
    runtime.full_access_warning_shown();
    runtime.full_access_warning_shown();
    assert_eq!(
        fixture.take(),
        [
            "error|full-access-acknowledgment|active for this process but not saved to user settings (InvalidSettingsFormat)"
        ]
    );
    fs::write(fixture.settings_file(), "{}").unwrap();
    runtime.full_access_warning_shown();
    runtime.handle_command("full-access");
    assert_eq!(
        fixture.saved().as_deref(),
        Some("{\"permission_mode\":\"yolo\"}\n")
    );
    assert_eq!(
        fixture.take(),
        [
            "mode yolo warn=true",
            "warning|permissions|mode set to full access",
        ]
    );
}
