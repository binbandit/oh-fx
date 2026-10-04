use ofx_contract::{StatuslineItem, UiCommand, UiEvent};

use crate::shell::test_shell::TestShell;

const HINT: &str = "↑↓ navigate     ←→ change     esc close";

fn opened() -> TestShell {
    let mut test = TestShell::start();
    test.deliver(UiEvent::StatuslineMenuOpened);
    test
}

fn press(test: &mut TestShell, bytes: &[u8]) -> String {
    test.type_bytes(bytes);
    test.step();
    test.screen()
}

fn toggle(item: StatuslineItem) -> UiCommand {
    UiCommand::ToggleStatusline { item }
}

fn shown(test: &TestShell, item: StatuslineItem) -> bool {
    test.shell.statusline.toggles().enabled(item)
}

fn changed(test: &mut TestShell, item: StatuslineItem, enabled: bool) {
    test.deliver(UiEvent::StatuslineChanged { item, enabled });
}

#[test]
fn the_menu_takes_the_composer_s_place_and_lists_the_three_segments_with_their_values() {
    let mut test = opened();
    let screen = test.screen();
    let title = screen.find("Status line\n\n").unwrap();
    let context = screen.find("  Context      off  on\n").unwrap();
    let session = screen.find("  Session      off  on\n").unwrap();
    let workspace = screen.find("  Workspace    off  on\n").unwrap();
    let hint = screen.find(HINT).unwrap();
    assert!(title < context && context < session, "{screen}");
    assert!(session < workspace && workspace < hint, "{screen}");
    assert!(!screen.contains("auto · model-a"), "{screen}");
    assert!(!screen.contains('┃'), "{screen}");
    assert!(test.cursor_hidden());
}

#[test]
fn toggles_in_one_batch_each_reach_the_controller_and_flip_the_shown_value() {
    let mut test = opened();
    press(&mut test, b"\r\r\x1b[B\x1b[C");
    assert_eq!(
        test.sent(),
        [
            toggle(StatuslineItem::Context),
            toggle(StatuslineItem::Context),
            toggle(StatuslineItem::Session),
        ]
    );
    let screen = test.screen();
    assert!(screen.contains("  Session      off  on"), "{screen}");
    assert!(shown(&test, StatuslineItem::Session));
    assert!(!shown(&test, StatuslineItem::Context));
}

#[test]
fn enter_left_and_right_toggle_the_selected_segment() {
    let mut test = opened();
    press(&mut test, b"\x1b[B\r");
    assert_eq!(test.sent().last(), Some(&toggle(StatuslineItem::Session)));
    assert!(shown(&test, StatuslineItem::Session));
    press(&mut test, b"\x1b[B\x1b[C");
    assert_eq!(test.sent().last(), Some(&toggle(StatuslineItem::Workspace)));
    press(&mut test, b"\x1b[B\x1b[D");
    assert_eq!(test.sent().last(), Some(&toggle(StatuslineItem::Context)));
    assert_eq!(test.sent().len(), 3);
}

#[test]
fn every_toggle_reaches_the_controller_when_its_changes_arrive_between_keys() {
    let mut test = opened();
    press(&mut test, b"\r\r");
    changed(&mut test, StatuslineItem::Context, true);
    press(&mut test, b"\r");
    assert_eq!(
        test.sent(),
        [
            toggle(StatuslineItem::Context),
            toggle(StatuslineItem::Context),
            toggle(StatuslineItem::Context),
        ]
    );
    changed(&mut test, StatuslineItem::Context, false);
    changed(&mut test, StatuslineItem::Context, true);
    assert!(shown(&test, StatuslineItem::Context));
}

#[test]
fn a_slash_command_s_change_reaches_the_open_menu_and_the_next_toggle_flips_it() {
    let mut test = opened();
    changed(&mut test, StatuslineItem::Context, true);
    assert!(shown(&test, StatuslineItem::Context));
    press(&mut test, b"\r");
    assert_eq!(test.sent(), [toggle(StatuslineItem::Context)]);
    assert!(!shown(&test, StatuslineItem::Context));
    changed(&mut test, StatuslineItem::Context, false);
    press(&mut test, b"\x1b");
    test.advance(100);
    test.settle();
    assert!(test.shell.statusline_menu.is_none());
    assert!(!shown(&test, StatuslineItem::Context));
}

#[test]
fn ctrl_j_and_ctrl_k_move_and_other_keys_never_reach_the_composer() {
    let mut test = opened();
    press(&mut test, b"\n\nx\x03\x04 \r");
    assert_eq!(test.sent().last(), Some(&toggle(StatuslineItem::Workspace)));
    press(&mut test, b"\x0b\r");
    assert_eq!(test.sent().last(), Some(&toggle(StatuslineItem::Session)));
    assert!(test.shell.composer.is_empty());
    assert!(test.shell.statusline_menu.is_some());
    assert!(!test.shell.gestures.ctrl_c_exit_armed());
}

#[test]
fn escape_closes_the_menu_and_the_status_line_returns() {
    let mut test = opened();
    press(&mut test, b"\x1b");
    test.advance(100);
    test.settle();
    let screen = test.screen();
    assert!(test.shell.statusline_menu.is_none());
    assert!(!screen.contains("Status line"), "{screen}");
    assert!(screen.contains("auto · model-a"), "{screen}");
    assert!(screen.contains("┃ "), "{screen}");
    assert!(!test.cursor_hidden());
}

#[test]
fn a_short_terminal_keeps_the_selected_segment_in_view() {
    let mut test = opened();
    test.resize(9, 80);
    press(&mut test, b"\x1b[B\x1b[B");
    let screen = test.screen();
    assert!(screen.contains("  Workspace    off  on"), "{screen}");
}
