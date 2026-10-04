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

fn set(item: StatuslineItem, enabled: bool) -> UiCommand {
    UiCommand::SetStatusline { item, enabled }
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
fn toggles_in_one_batch_each_flip_the_value_the_last_one_left() {
    let mut test = opened();
    press(&mut test, b"\r\r\x1b[B\x1b[C");
    assert_eq!(
        test.sent(),
        [
            set(StatuslineItem::Context, true),
            set(StatuslineItem::Context, false),
            set(StatuslineItem::Session, true),
        ]
    );
    let screen = test.screen();
    assert!(screen.contains("  Session      off  on"), "{screen}");
    assert!(
        test.shell
            .statusline
            .toggles()
            .enabled(StatuslineItem::Session)
    );
    assert!(
        !test
            .shell
            .statusline
            .toggles()
            .enabled(StatuslineItem::Context)
    );
}

#[test]
fn enter_toggles_the_selected_segment_and_the_row_follows_its_value() {
    let mut test = opened();
    press(&mut test, b"\x1b[B\r");
    assert_eq!(
        test.sent().last(),
        Some(&set(StatuslineItem::Session, true))
    );
    test.deliver(UiEvent::StatuslineChanged {
        item: StatuslineItem::Session,
        enabled: true,
    });
    press(&mut test, b"\x1b[B\x1b[C");
    assert_eq!(
        test.sent().last(),
        Some(&set(StatuslineItem::Workspace, true))
    );
    press(&mut test, b"\x1b[B\x1b[D");
    assert_eq!(
        test.sent().last(),
        Some(&set(StatuslineItem::Context, true))
    );
    press(&mut test, b"\x1b[A\x1b[A\x1b[D");
    assert_eq!(
        test.sent().last(),
        Some(&set(StatuslineItem::Session, false))
    );
}

#[test]
fn an_echo_of_an_older_toggle_never_undoes_a_newer_one_while_the_menu_is_open() {
    let mut test = opened();
    press(&mut test, b"\r\r");
    test.deliver(UiEvent::StatuslineChanged {
        item: StatuslineItem::Context,
        enabled: true,
    });
    press(&mut test, b"\r");
    assert_eq!(
        test.sent(),
        [
            set(StatuslineItem::Context, true),
            set(StatuslineItem::Context, false),
            set(StatuslineItem::Context, true),
        ]
    );
    test.deliver(UiEvent::StatuslineChanged {
        item: StatuslineItem::Context,
        enabled: false,
    });
    assert!(
        test.shell
            .statusline
            .toggles()
            .enabled(StatuslineItem::Context)
    );
}

#[test]
fn ctrl_j_and_ctrl_k_move_and_other_keys_never_reach_the_composer() {
    let mut test = opened();
    press(&mut test, b"\n\nx\x03\x04 \r");
    assert_eq!(
        test.sent().last(),
        Some(&set(StatuslineItem::Workspace, true))
    );
    press(&mut test, b"\x0b\r");
    assert_eq!(
        test.sent().last(),
        Some(&set(StatuslineItem::Session, true))
    );
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
