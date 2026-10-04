use ofx_contract::{
    FastModeSetting, ModelCatalog, ModelCatalogSource, PermissionMode, SettingId, SettingsSnapshot,
    StatuslineItem, StatuslineToggles, UiCommand, UiEvent,
};

use std::cell::RefCell;
use std::rc::Rc;

use crate::shell::PromptHistory;
use crate::shell::model_menu::tests::option;
use crate::shell::test_shell::TestShell;

const HINT: &str = "↑↓ navigate     tab category     ←→ change     esc close";

fn snapshot() -> SettingsSnapshot {
    SettingsSnapshot {
        model: "model-a".to_owned(),
        effort: "default".to_owned(),
        reasoning_efforts: Vec::new(),
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

fn listed(test: &mut TestShell, ids: &[&str]) {
    test.deliver(UiEvent::ModelCatalog {
        provider: "local".to_owned(),
        catalog: ModelCatalog::Listed {
            models: ids.iter().map(|id| option(id)).collect(),
            source: ModelCatalogSource::ProfileSettings,
        },
    });
}

fn picked(model: &str) -> UiCommand {
    UiCommand::SelectModelFromSettings {
        model: model.to_owned(),
    }
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
        .find("Settings 10  [All]  Interface  Agent  Notifications  Advanced")
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
    press(&mut test, b"\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[D");
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
fn enter_on_the_model_row_lists_the_models_under_it() {
    let mut test = opened();
    let screen = press(&mut test, b"\r\x1b[B\x1b[B\x1b[B\r");
    assert_eq!(test.sent(), [UiCommand::ListModels]);
    assert!(test.shell.settings_menu.is_some());
    let model = screen.find("  Model                    model-a").unwrap();
    let loading = screen.find("\n    Loading models…").unwrap();
    assert!(model < loading, "{screen}");
    listed(&mut test, &["vendor/one", "vendor/two"]);
    let screen = test.screen();
    assert!(
        screen.contains(
            "  Model                    model-a\n                           vendor/one\n                           vendor/two\n  Reasoning effort         default"
        ),
        "{screen}"
    );
}

#[test]
fn enter_picks_the_highlighted_model_and_returns_to_the_settings_search() {
    let mut test = opened();
    press(&mut test, b"model\r");
    listed(&mut test, &["vendor/one", "vendor/two", "vendor/three"]);
    let screen = press(&mut test, b"\x1b[H\x1b[B");
    assert!(screen.contains("vendor/three"), "{screen}");
    press(&mut test, b"\r");
    assert_eq!(test.sent(), [UiCommand::ListModels, picked("vendor/two")]);
    assert!(test.shell.model_menu.is_none());
    assert!(test.shell.settings_menu.is_some());
    assert_eq!(test.shell.composer.text(), "model");
    let screen = test.screen();
    assert!(!screen.contains("vendor/"), "{screen}");
    assert!(screen.contains("  Model "), "{screen}");
}

#[test]
fn right_opens_the_models_left_picks_one_and_tab_closes_them() {
    let mut test = opened();
    press(&mut test, b"\x1b[B\x1b[B\x1b[B\x1b[C");
    assert!(test.shell.model_menu.is_some());
    listed(&mut test, &["vendor/one", "vendor/two"]);
    press(&mut test, b"\x1b[A\x1b[C");
    assert!(test.shell.model_menu.is_some());
    press(&mut test, b"\x1b[D");
    assert_eq!(test.sent(), [UiCommand::ListModels, picked("vendor/two")]);
    assert!(test.shell.model_menu.is_none());
    press(&mut test, b"\x1b[C");
    assert!(test.shell.model_menu.is_some());
    let screen = press(&mut test, b"\t");
    assert!(test.shell.model_menu.is_none());
    assert!(screen.contains("[Interface]"), "{screen}");
    assert_eq!(test.sent().len(), 2, "{:?}", test.sent());
}

#[test]
fn the_effort_row_steps_through_the_model_s_efforts() {
    let mut test = opened();
    test.deliver(UiEvent::SettingsChanged {
        snapshot: SettingsSnapshot {
            reasoning_efforts: vec!["low".to_owned(), "high".to_owned()],
            ..snapshot()
        },
    });
    let screen = press(&mut test, b"\x1b[B\x1b[B\x1b[B\x1b[B\x1b[C");
    assert!(
        screen.contains("  Reasoning effort         default  low  high"),
        "{screen}"
    );
    assert_eq!(test.sent(), [step(SettingId::Effort, 1)]);
    let mut test = opened();
    press(&mut test, b"\x1b[B\x1b[B\x1b[B\x1b[B\x1b[C");
    assert!(test.sent().is_empty(), "{:?}", test.sent());
}

#[test]
fn escape_closes_the_settings_before_the_inline_models() {
    let mut test = opened();
    press(&mut test, b"model\r");
    press(&mut test, b"\x1b");
    test.advance(100);
    test.settle();
    assert!(test.shell.settings_menu.is_none());
    assert!(test.shell.model_menu.is_some());
    assert!(test.shell.composer.is_empty());
}

#[test]
fn ctrl_p_closes_the_inline_models_and_clears_the_search() {
    let mut test = opened();
    press(&mut test, b"model\r");
    press(&mut test, b"\x10");
    assert!(test.shell.model_menu.is_none());
    assert!(test.shell.settings_menu.is_some());
    assert!(test.shell.composer.is_empty());
}

#[test]
fn tab_and_shift_tab_cycle_the_categories_and_reset_the_selection() {
    let mut test = opened();
    press(&mut test, b"\x1b[B");
    let screen = press(&mut test, b"\t\t");
    assert!(
        screen.contains("Settings 5  All  Interface  [Agent]"),
        "{screen}"
    );
    press(&mut test, b"\x1b[B\x1b[C");
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

fn recorded() -> (TestShell, Rc<RefCell<Vec<String>>>) {
    let saved = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&saved);
    let history = PromptHistory::enabled(Vec::new(), move |text| {
        sink.borrow_mut().push(text.to_owned());
        Ok(())
    });
    let mut test = TestShell::start_with(|options| options.prompt_history = history);
    test.deliver(UiEvent::SettingsMenuOpened {
        snapshot: snapshot(),
    });
    press(&mut test, b"history");
    (test, saved)
}

fn history_changed(test: &mut TestShell, enabled: bool) {
    test.deliver(UiEvent::PromptHistoryChanged { enabled });
}

#[test]
fn switching_prompt_history_off_stops_recording_before_the_controller_answers() {
    let (mut test, saved) = recorded();
    press(&mut test, b"\x1b[C\x1b");
    test.advance(100);
    test.settle();
    test.submit("private");
    assert!(saved.borrow().is_empty());
    assert_eq!(
        test.sent().first(),
        Some(&step(SettingId::PromptHistory, 1))
    );
    history_changed(&mut test, false);
    test.submit("still private");
    assert!(saved.borrow().is_empty());
}

#[test]
fn quick_prompt_history_steps_record_only_once_the_last_answer_arrives() {
    let (mut test, saved) = recorded();
    press(&mut test, b"\x1b[C\x1b[C\x1b[C");
    history_changed(&mut test, false);
    history_changed(&mut test, true);
    press(&mut test, b"\x1b");
    test.advance(100);
    test.settle();
    test.submit("between");
    history_changed(&mut test, false);
    test.submit("after");
    assert!(saved.borrow().is_empty());
    let (mut test, saved) = recorded();
    press(&mut test, b"\x1b[C\x1b[C");
    history_changed(&mut test, false);
    press(&mut test, b"\x1b");
    test.advance(100);
    test.settle();
    test.submit("waiting");
    history_changed(&mut test, true);
    test.submit("kept");
    assert_eq!(*saved.borrow(), ["kept"]);
}

#[test]
fn super_r_leaves_the_menu_in_charge() {
    let mut test = opened();
    press(&mut test, b"\x1b[114;9u");
    assert!(test.shell.settings_menu.is_some());
    assert!(test.shell.picker.is_none());
    assert!(test.sent().is_empty(), "{:?}", test.sent());
}

#[test]
fn a_menu_key_after_ctrl_c_disarms_the_exit() {
    let mut test = opened();
    press(&mut test, b"\x03");
    assert!(test.shell.gestures.ctrl_c_exit_armed());
    press(&mut test, b"\x1b[B");
    assert!(!test.shell.gestures.ctrl_c_exit_armed());
    press(&mut test, b"\x03\x1b[C");
    assert!(!test.shell.gestures.ctrl_c_exit_armed());
    press(&mut test, b"\x03\t");
    assert!(!test.shell.gestures.ctrl_c_exit_armed());
    press(&mut test, b"\x03\x1b");
    test.advance(100);
    test.settle();
    assert!(!test.shell.gestures.ctrl_c_exit_armed());
}
