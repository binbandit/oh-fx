use ofx_contract::{
    FastModeSetting, PermissionMode, SettingId, SettingsSnapshot, StatuslineItem,
    StatuslineToggles, UiCommand, UiEvent,
};

use crate::shell::test_shell::TestShell;

const HINT: &str = "↑↓ navigate     tab category     ←→ change     esc close";

fn snapshot() -> SettingsSnapshot {
    SettingsSnapshot {
        model: "model-a".to_owned(),
        fast_mode: FastModeSetting::Off,
        permission_mode: PermissionMode::Auto,
        statusline: StatuslineToggles::default(),
        session_titles: true,
        startup_scrollback: true,
        prompt_history: true,
    }
}

fn opened() -> TestShell {
    let mut test = TestShell::start();
    test.deliver(UiEvent::SettingsMenuOpened {
        snapshot: snapshot(),
    });
    test
}

fn press(test: &mut TestShell, bytes: &[u8]) -> String {
    test.type_bytes(bytes);
    test.step();
    test.screen()
}

fn step(setting: SettingId, delta: isize) -> UiCommand {
    UiCommand::StepSetting { setting, delta }
}

fn shown(test: &TestShell) -> SettingsSnapshot {
    test.shell
        .settings_menu
        .as_ref()
        .map(|menu| menu.snapshot.clone())
        .unwrap()
}

#[test]
fn the_menu_lists_every_setting_under_the_composer_in_place_of_the_status_line() {
    let mut test = opened();
    let screen = test.screen();
    let composer = screen.find("┃ ").unwrap();
    let header = screen
        .find("Settings 9  [All]  Interface  Agent  Notifications  Advanced")
        .unwrap();
    let first = screen.find("  Status line context      off  on").unwrap();
    let model = screen.find("  Model                    model-a").unwrap();
    let last = screen.find("  Prompt history           off  on").unwrap();
    let hint = screen.find(HINT).unwrap();
    assert!(
        composer < header && header < first && first < model,
        "{screen}"
    );
    assert!(model < last && last < hint, "{screen}");
    assert!(!screen.contains("auto · model-a"), "{screen}");
}

#[test]
fn left_and_right_send_a_step_and_the_row_follows_the_controller_s_snapshot() {
    let mut test = opened();
    press(&mut test, b"\x1b[C");
    assert_eq!(
        test.sent().last(),
        Some(&step(SettingId::StatuslineContext, 1))
    );
    press(&mut test, b"\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[D");
    assert_eq!(
        test.sent().last(),
        Some(&step(SettingId::PermissionMode, -1))
    );
    test.deliver(UiEvent::SettingsChanged {
        snapshot: SettingsSnapshot {
            permission_mode: PermissionMode::Ask,
            ..snapshot()
        },
    });
    assert_eq!(shown(&test).permission_mode, PermissionMode::Ask);
}

#[test]
fn two_quick_presses_on_one_row_each_step_the_value_the_controller_holds() {
    let mut test = opened();
    press(&mut test, b"\x1b[C\x1b[C");
    assert_eq!(
        test.sent(),
        [
            step(SettingId::StatuslineContext, 1),
            step(SettingId::StatuslineContext, 1),
        ]
    );
    let context = |enabled| {
        let mut statusline = StatuslineToggles::default();
        statusline.set(StatuslineItem::Context, enabled);
        UiEvent::SettingsChanged {
            snapshot: SettingsSnapshot {
                statusline,
                ..snapshot()
            },
        }
    };
    test.deliver(context(true));
    test.deliver(context(false));
    assert!(!shown(&test).statusline.enabled(StatuslineItem::Context));
}

#[test]
fn the_model_row_and_enter_send_nothing() {
    let mut test = opened();
    press(&mut test, b"\x1b[B\x1b[B\x1b[B\x1b[C\x1b[D\r");
    assert!(test.sent().is_empty(), "{:?}", test.sent());
    assert!(test.shell.settings_menu.is_some());
}

#[test]
fn tab_and_shift_tab_cycle_the_categories_and_reset_the_selection() {
    let mut test = opened();
    press(&mut test, b"\x1b[B");
    let screen = press(&mut test, b"\t\t");
    assert!(
        screen.contains("Settings 4  All  Interface  [Agent]"),
        "{screen}"
    );
    press(&mut test, b"\x1b[C");
    assert!(test.sent().is_empty(), "{:?}", test.sent());
    press(&mut test, b"\x1b[B\x1b[C");
    assert_eq!(test.sent().last(), Some(&step(SettingId::FastMode, 1)));
    let screen = press(&mut test, b"\x1b[Z\x1b[Z\x1b[Z");
    assert!(screen.contains("[Advanced]"), "{screen}");
    assert!(
        screen.contains("  Startup scrollback    off  on"),
        "{screen}"
    );
    let screen = press(&mut test, b"\x1b[Z");
    assert!(screen.contains("[Notifications]"), "{screen}");
    assert!(screen.contains("No settings found."), "{screen}");
}

#[test]
fn typing_filters_the_rows_and_moves_wrap_within_the_matches() {
    let mut test = opened();
    let screen = press(&mut test, b"status");
    assert!(screen.contains("Settings 3"), "{screen}");
    assert!(!screen.contains("Prompt history"), "{screen}");
    press(&mut test, b"\x1b[A\x1b[C");
    assert_eq!(
        test.sent().last(),
        Some(&step(SettingId::StatuslineWorkspace, 1))
    );
    press(&mut test, b"\x1b[B\x1b[C");
    assert_eq!(
        test.sent().last(),
        Some(&step(SettingId::StatuslineContext, 1))
    );
    assert_eq!(test.shell.composer.text(), "status");
}

#[test]
fn escape_closes_the_menu_and_clears_the_search() {
    let mut test = opened();
    press(&mut test, b"prompt");
    press(&mut test, b"\x1b");
    test.advance(100);
    test.settle();
    let screen = test.screen();
    assert!(test.shell.settings_menu.is_none());
    assert!(test.shell.composer.is_empty());
    assert!(!screen.contains("Settings 1"), "{screen}");
    assert!(screen.contains("auto · model-a"), "{screen}");
}

#[test]
fn ctrl_p_and_a_typed_model_command_leave_the_menu_in_charge() {
    let mut test = opened();
    press(&mut test, b"\x10");
    press(&mut test, b"/model gpt ");
    assert!(test.shell.settings_menu.is_some());
    assert!(test.shell.model_menu.is_none());
    assert!(test.shell.model_draft.is_none());
    assert_eq!(test.shell.composer.text(), "/model gpt ");
    assert!(!test.sent().contains(&UiCommand::ListModels));
}

#[test]
fn a_pending_ctrl_c_shows_in_the_hint() {
    let mut test = opened();
    let screen = press(&mut test, b"\x03");
    assert!(test.shell.settings_menu.is_some());
    assert!(screen.contains("press ctrl+c again to exit"), "{screen}");
}

#[test]
fn a_short_terminal_keeps_the_selected_setting_in_view() {
    let mut test = opened();
    test.resize(12, 80);
    let screen = press(&mut test, b"\x1b[A");
    assert!(screen.contains("  Prompt history"), "{screen}");
    assert!(!screen.contains("  Status line context"), "{screen}");
}
