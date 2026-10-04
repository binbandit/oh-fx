use std::path::PathBuf;

use ofx_contract::{DirectoryAccess, UiCommand, UiEvent, WorkspaceMenu, WorkspaceMenuEntry};

use crate::shell::test_shell::TestShell;

const HINT: &str = "↑↓ navigate     enter use     esc close";

fn entry(path: &str, saved: bool, command_line: bool) -> WorkspaceMenuEntry {
    WorkspaceMenuEntry {
        path: PathBuf::from(path),
        saved,
        command_line,
        access: DirectoryAccess::Active,
    }
}

fn opened() -> TestShell {
    let mut test = TestShell::start();
    test.deliver(UiEvent::WorkspaceMenuOpened {
        menu: WorkspaceMenu {
            primary: PathBuf::from("/tmp/project"),
            saved_suppressed: false,
            limit: 16,
            entries: vec![
                entry("/tmp/launch", false, true),
                entry("/tmp/shared dir", true, false),
            ],
        },
    });
    test
}

fn press(test: &mut TestShell, bytes: &[u8]) -> String {
    test.type_bytes(bytes);
    test.step();
    test.screen()
}

#[test]
fn the_menu_takes_the_composer_s_place_with_the_summary_and_every_directory() {
    let mut test = opened();
    let screen = test.screen();
    let order = [
        "Workspace\n\n",
        "  Primary",
        "/tmp/project",
        "  Additional directories",
        "2 / 16",
        "❯ Add directory…",
        "  /tmp/launch",
        "Active · Launch only",
        "  /tmp/shared dir",
        "  Clear saved directories",
        HINT,
    ];
    let mut from = 0;
    for needle in order {
        let found = screen[from..]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {screen}"));
        from += found + needle.len();
    }
    assert!(!screen.contains('┃'), "{screen}");
    assert!(test.cursor_hidden());
}

#[test]
fn enter_prepares_the_selected_command_in_the_composer_without_running_it() {
    for (keys, prepared) in [
        (&b"\r"[..], "/workspace add "),
        (b"\x1b[B\r", "/workspace remove /tmp/shared dir"),
        (b"\x1b[B\x1b[B\r", "/workspace clear"),
        (b"\x1b[A\r", "/workspace clear"),
    ] {
        let mut test = opened();
        let screen = press(&mut test, keys);
        assert!(test.shell.workspace_menu.is_none(), "{keys:?}");
        assert_eq!(test.shell.composer.text(), prepared, "{keys:?}");
        assert!(test.sent().is_empty(), "{keys:?}");
        assert!(!screen.contains(HINT), "{screen}");
    }
}

#[test]
fn launch_only_directories_are_shown_but_never_selected() {
    let mut test = opened();
    let screen = press(&mut test, b"\x1b[B");
    assert!(screen.contains("❯ /tmp/shared dir"), "{screen}");
    assert!(!screen.contains("❯ /tmp/launch"), "{screen}");
}

#[test]
fn escape_closes_the_menu_and_other_keys_never_reach_the_composer() {
    let mut test = opened();
    press(&mut test, b"x \x03\n\x0b");
    assert!(test.shell.composer.is_empty());
    assert!(test.shell.workspace_menu.is_some());
    press(&mut test, b"\x1b");
    test.advance(100);
    test.settle();
    assert!(test.shell.workspace_menu.is_none());
    assert_eq!(test.sent(), Vec::<UiCommand>::new());
}

#[test]
fn opening_another_compact_menu_replaces_it() {
    let mut test = opened();
    test.deliver(UiEvent::StatuslineMenuOpened);
    assert!(test.shell.workspace_menu.is_none());
    assert!(test.shell.statusline_menu.is_some());
    test.deliver(UiEvent::WorkspaceMenuOpened {
        menu: WorkspaceMenu {
            primary: PathBuf::from("/tmp/project"),
            saved_suppressed: false,
            limit: 16,
            entries: Vec::new(),
        },
    });
    assert!(test.shell.statusline_menu.is_none());
    assert!(test.screen().contains("0 / 16"));
}

#[test]
fn a_prepared_command_opens_no_picker_over_the_composer() {
    for (keys, prepared) in [
        (&b"\x1b[B\r"[..], "/workspace remove /tmp/shared dir"),
        (b"\r", "/workspace add "),
    ] {
        let mut test = opened();
        press(&mut test, keys);
        test.advance(100);
        test.settle();
        let screen = test.screen();
        assert!(!screen.contains('❯'), "{screen}");
        assert!(
            screen.contains(&format!("┃ {}", prepared.trim_end())),
            "{screen}"
        );
        assert_eq!(test.shell.composer.text(), prepared);
    }
}
