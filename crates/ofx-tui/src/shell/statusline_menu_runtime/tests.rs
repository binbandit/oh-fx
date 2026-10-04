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
fn the_menu_lists_the_three_segments_under_the_composer_with_their_values() {
    let mut test = opened();
    let screen = test.screen();
    let composer = screen.find("┃ ").unwrap();
    let title = screen.find("Status line\n\n").unwrap();
    let context = screen.find("  Context      off  on\n").unwrap();
    let session = screen.find("  Session      off  on\n").unwrap();
    let workspace = screen.find("  Workspace    off  on\n").unwrap();
    let hint = screen.find(HINT).unwrap();
    assert!(
        composer < title && title < context && context < session,
        "{screen}"
    );
    assert!(session < workspace && workspace < hint, "{screen}");
    assert!(!screen.contains("auto · model-a"), "{screen}");
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
}

#[test]
fn a_short_terminal_keeps_the_selected_segment_in_view() {
    let mut test = opened();
    test.resize(9, 80);
    press(&mut test, b"\x1b[B\x1b[B");
    let screen = test.screen();
    assert!(screen.contains("  Workspace    off  on"), "{screen}");
}
