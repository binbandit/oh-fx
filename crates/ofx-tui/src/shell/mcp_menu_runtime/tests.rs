use ofx_contract::{
    McpMenuAuthentication, McpMenuConnection, McpMenuCounts, McpMenuRequest, McpMenuServer,
    McpMenuSnapshot, McpMenuSource, McpMenuStatus, McpMenuTransport, UiCommand, UiEvent,
    UsageMenuUpdate, UsageScope,
};

use super::super::mcp_menu::Screen;
use crate::shell::test_shell::TestShell;

const ROOT_HINT: &str = "↑↓ move  enter inspect  c help  esc close";

fn server(name: &str, transport: McpMenuTransport, status: McpMenuStatus) -> McpMenuServer {
    McpMenuServer {
        name: name.to_owned(),
        source: McpMenuSource::Profile,
        admission: None,
        transport,
        required: false,
        connection: if status == McpMenuStatus::Ready {
            McpMenuConnection::Ready
        } else {
            McpMenuConnection::Disconnected
        },
        authentication: McpMenuAuthentication::None,
        status,
        reloading: false,
        protocol_version: None,
        counts: McpMenuCounts::default(),
        failure: None,
    }
}

fn servers(names: &[&str]) -> McpMenuSnapshot {
    McpMenuSnapshot {
        servers: names
            .iter()
            .map(|name| match *name {
                "remote" => server(name, McpMenuTransport::Http, McpMenuStatus::Unavailable),
                name => server(name, McpMenuTransport::Stdio, McpMenuStatus::Ready),
            })
            .collect(),
        configuration_issues: Vec::new(),
        profile_config: Some("~/.config/oh-fx/mcp.json".to_owned()),
    }
}

fn opened(names: &[&str]) -> TestShell {
    let mut test = TestShell::start();
    test.deliver(UiEvent::McpMenuOpened {
        snapshot: servers(names),
    });
    test
}

fn press(test: &mut TestShell, bytes: &[u8]) -> String {
    test.type_bytes(bytes);
    test.step();
    test.screen()
}

fn escape(test: &mut TestShell) -> String {
    test.type_bytes(b"\x1b");
    test.step();
    test.advance(100);
    test.settle();
    test.screen()
}

fn refresh_now(test: &mut TestShell) {
    test.draining(|shell| shell.refresh_mcp_menu(shell.now_ms()));
}

fn menu(request: McpMenuRequest) -> UiCommand {
    UiCommand::McpMenu { request }
}

#[test]
fn the_menu_lists_servers_above_the_composer_with_its_hint() {
    let mut test = opened(&["docs", "remote"]);
    let screen = test.screen();
    for needle in [
        "MCP 2  [Servers]",
        "  docs                  Ready                 stdio · Profile",
        "  remote                Disconnected          HTTP · Profile",
        ROOT_HINT,
    ] {
        assert!(screen.contains(needle), "{needle:?} in {screen}");
    }
    let screen = press(&mut test, b"xyz");
    assert!(test.shell.composer.text().is_empty(), "{screen}");
    assert!(test.sent().is_empty());
}

#[test]
fn enter_inspects_the_selected_server() {
    let mut test = opened(&["docs", "remote"]);
    let screen = press(&mut test, b"\r");
    assert!(screen.contains("  Server              docs"), "{screen}");
    assert!(screen.contains("c help  esc back"), "{screen}");
    press(&mut test, b"\r");
    let screen = press(&mut test, b"\x1b[B");
    assert!(screen.contains("  Server              remote"), "{screen}");
    assert!(!screen.contains("enter sign in"), "{screen}");
    press(&mut test, b"r");
    assert!(test.sent().is_empty());
}

#[test]
fn help_and_escape_walk_back_out_of_the_menu() {
    let mut test = opened(&["docs"]);
    let screen = press(&mut test, b"c");
    assert!(
        screen.contains("  Profile config      ~/.config/oh-fx/mcp.json"),
        "{screen}"
    );
    assert!(screen.contains("esc back"), "{screen}");
    let screen = escape(&mut test);
    assert!(screen.contains(ROOT_HINT), "{screen}");
    let screen = escape(&mut test);
    assert!(test.shell.mcp_menu.is_none());
    assert!(!screen.contains("MCP 1"), "{screen}");
}

#[test]
fn the_open_menu_refreshes_its_servers_every_tenth_of_a_second() {
    let mut test = opened(&["docs", "remote"]);
    press(&mut test, b"\x1b[B\r");
    test.advance(99);
    refresh_now(&mut test);
    assert!(test.sent().is_empty());
    test.advance(1);
    refresh_now(&mut test);
    assert_eq!(test.sent(), [menu(McpMenuRequest::Refresh)]);
    test.advance(500);
    refresh_now(&mut test);
    assert_eq!(test.sent().len(), 1);
    assert_eq!(test.shell.mcp_menu_refresh_due_ms(), None);
    test.deliver(UiEvent::McpMenuRefreshed {
        snapshot: servers(&["added", "docs", "remote"]),
    });
    let menu_state = &test.shell.mcp_menu.as_ref().unwrap().state;
    assert_eq!(menu_state.selected_server_index, 2);
    assert_eq!(menu_state.screen, Screen::Details);
    test.advance(100);
    refresh_now(&mut test);
    assert_eq!(test.sent().len(), 2);
    test.deliver(UiEvent::McpMenuRefreshed {
        snapshot: servers(&["docs"]),
    });
    let menu_state = &test.shell.mcp_menu.as_ref().unwrap().state;
    assert_eq!(menu_state.selected_server_index, 0);
    assert_eq!(menu_state.screen, Screen::Browse);
    escape(&mut test);
    assert_eq!(test.shell.mcp_menu_refresh_due_ms(), None);
    test.advance(1000);
    refresh_now(&mut test);
    assert_eq!(test.sent().len(), 2);
}

#[test]
fn the_menu_replaces_and_yields_to_the_other_footer_menus() {
    let mut test = opened(&["docs"]);
    test.deliver(UiEvent::StatuslineMenuOpened);
    assert!(test.shell.mcp_menu.is_none());
    assert!(test.shell.statusline_menu.is_some());
    test.deliver(UiEvent::McpMenuOpened {
        snapshot: servers(&["docs"]),
    });
    assert!(test.shell.statusline_menu.is_none());
    assert!(test.shell.mcp_menu.is_some());
    test.deliver(UiEvent::UsageMenu {
        update: UsageMenuUpdate::Opened {
            scope: UsageScope::Days30,
            report: None,
        },
    });
    assert!(test.shell.mcp_menu.is_none());
    assert!(test.shell.usage_menu.is_some());
    test.deliver(UiEvent::McpMenuOpened {
        snapshot: servers(&["docs"]),
    });
    assert!(test.shell.usage_menu.is_none());
    assert!(test.shell.mcp_menu.is_some());
}

#[test]
fn the_menu_keeps_ctrl_c_and_the_model_shortcut_from_acting_behind_it() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\x1b[200~draft\x1b[201~");
    press(&mut test, b"\x03");
    assert!(!test.shell.gestures.ctrl_c_exit_armed());
    assert_eq!(test.shell.composer.text(), "draft");
    press(&mut test, b"\x10");
    assert!(test.shell.model_menu.is_none());
    assert!(test.shell.model_draft.is_none());
    assert!(test.shell.mcp_menu.is_some());
    assert!(
        !test
            .sent()
            .iter()
            .any(|command| matches!(command, UiCommand::ListModels))
    );
}

#[test]
fn closing_the_menu_clears_what_was_pasted_while_it_was_open() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\x1b[200~pasted\x1b[201~");
    assert_eq!(test.shell.composer.text(), "pasted");
    let screen = escape(&mut test);
    assert!(test.shell.mcp_menu.is_none(), "{screen}");
    assert!(test.shell.composer.is_empty());
}
