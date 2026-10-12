use ofx_contract::{
    McpMenuAuthentication, McpMenuConnection, McpMenuCounts, McpMenuRequest, McpMenuServer,
    McpMenuSettled, McpMenuSnapshot, McpMenuSource, McpMenuStatus, McpMenuTransport, UiCommand,
    UiEvent, UsageMenuUpdate, UsageScope,
};

use super::super::mcp_menu::{LoadState, Screen};
use crate::shell::test_shell::TestShell;

const ROOT_HINT: &str = "↑↓ move  enter inspect  r reload  c help  esc close";

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

fn settled(generation: u64, succeeded: bool, feedback: &str, names: &[&str]) -> UiEvent {
    UiEvent::McpMenuSettled {
        settled: McpMenuSettled {
            generation,
            succeeded,
            feedback: feedback.to_owned(),
        },
        snapshot: servers(names),
    }
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
fn enter_inspects_a_server_and_signs_in_only_where_upstream_offers_it() {
    let mut test = opened(&["docs", "remote"]);
    let screen = press(&mut test, b"\r");
    assert!(screen.contains("  Server              docs"), "{screen}");
    assert!(screen.contains("c help  esc back"), "{screen}");
    press(&mut test, b"\r");
    assert!(test.sent().is_empty());
    let screen = press(&mut test, b"\x1b[B");
    assert!(screen.contains("  Server              remote"), "{screen}");
    assert!(
        screen.contains("enter sign in  c help  esc back"),
        "{screen}"
    );
    press(&mut test, b"\r");
    assert_eq!(
        test.sent(),
        [menu(McpMenuRequest::Authenticate {
            generation: 1,
            server: "remote".to_owned(),
        })]
    );
    let state = &test.shell.mcp_menu.as_ref().unwrap().state;
    assert_eq!(state.load_state, LoadState::Loading);
    test.deliver(settled(7, true, "stale", &["docs", "remote"]));
    assert!(!test.screen().contains("stale"));
    test.deliver(settled(
        1,
        true,
        "Authenticated MCP server 'remote'.",
        &["docs", "remote"],
    ));
    let screen = test.screen();
    assert!(
        screen.contains("  Authenticated MCP server 'remote'."),
        "{screen}"
    );
    let state = &test.shell.mcp_menu.as_ref().unwrap().state;
    assert_eq!(state.load_state, LoadState::Ready);
    assert_eq!(state.screen, Screen::Details);
}

#[test]
fn reload_runs_from_the_server_list_and_shows_its_result() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\r");
    press(&mut test, b"r");
    assert!(test.sent().is_empty());
    escape(&mut test);
    press(&mut test, b"R");
    assert_eq!(
        test.sent().last(),
        Some(&menu(McpMenuRequest::Reload { generation: 1 }))
    );
    test.deliver(settled(
        1,
        false,
        "MCP reload failed: LockBusy",
        &["docs", "fresh"],
    ));
    let screen = test.screen();
    assert!(screen.contains("MCP 2  [Servers]"), "{screen}");
    assert!(screen.contains("  MCP reload failed: LockBusy"), "{screen}");
    assert!(screen.contains("fresh"), "{screen}");
    assert_eq!(
        test.shell.mcp_menu.as_ref().unwrap().state.load_state,
        LoadState::Failed
    );
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

struct OneSkill;

impl crate::shell::SkillCatalogSource for OneSkill {
    fn menu_items(&self) -> Vec<ofx_contract::SkillMenuItem> {
        vec![ofx_contract::SkillMenuItem {
            name: "review".to_owned(),
            description: "review workflow".to_owned(),
            path: std::path::PathBuf::from("/skills/review"),
            source: ofx_contract::SkillMenuSource::OhFx,
            group: ofx_contract::SkillMenuGroup::Workspace,
            scope: "oh-fx · Workspace".to_owned(),
            source_label: String::new(),
        }]
    }
}

#[test]
fn the_sessions_shortcut_waits_for_the_menu_to_close() {
    let mut test = opened(&["docs"]);
    let screen = press(&mut test, b"\x1b[114;9u");
    assert!(test.shell.mcp_menu.is_some(), "{screen}");
    assert!(
        !test
            .sent()
            .iter()
            .any(|command| matches!(command, UiCommand::OpenSessions { .. })),
        "{:?}",
        test.sent()
    );
    escape(&mut test);
    press(&mut test, b"\x1b[114;9u");
    assert!(
        test.sent()
            .iter()
            .any(|command| matches!(command, UiCommand::OpenSessions { .. }))
    );
}

#[test]
fn closing_the_menu_drops_a_skill_mention_pasted_while_it_was_open() {
    let mut test = TestShell::start_with(|options| {
        options.skill_catalog = Some(Box::new(OneSkill));
    });
    test.deliver(UiEvent::McpMenuOpened {
        snapshot: servers(&["docs"]),
    });
    press(&mut test, b"\x1b[200~$rev\x1b[201~");
    assert_eq!(test.shell.composer.text(), "$rev");
    let screen = escape(&mut test);
    assert!(test.shell.mcp_menu.is_none(), "{screen}");
    assert!(test.shell.composer.is_empty());
    assert!(!test.shell.skills_menu_visible(), "{screen}");
    assert!(!screen.contains("Skills 1"), "{screen}");
    press(&mut test, b"\r");
    assert!(
        !test
            .sent()
            .iter()
            .any(|command| matches!(command, UiCommand::Submit { .. })),
        "{:?}",
        test.sent()
    );
}

fn pending(test: &TestShell) -> (Option<u64>, LoadState, Screen) {
    let menu_state = &test.shell.mcp_menu.as_ref().unwrap().state;
    (
        menu_state.pending_generation,
        menu_state.load_state,
        menu_state.screen,
    )
}

#[test]
fn a_reopened_menu_ignores_what_the_closed_one_started() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"r");
    escape(&mut test);
    assert!(test.shell.mcp_menu.is_none());
    test.deliver(UiEvent::McpMenuOpened {
        snapshot: servers(&["docs"]),
    });
    press(&mut test, b"r");
    assert_eq!(
        test.sent(),
        [
            menu(McpMenuRequest::Reload { generation: 1 }),
            menu(McpMenuRequest::Reload { generation: 2 }),
        ]
    );
    test.deliver(settled(1, false, "MCP reload failed: stale", &["old"]));
    let screen = test.screen();
    assert!(!screen.contains("stale"), "{screen}");
    assert!(!screen.contains("old"), "{screen}");
    assert_eq!(
        pending(&test),
        (Some(2), LoadState::Loading, Screen::Browse)
    );
    test.deliver(settled(2, true, "MCP configuration reloaded.", &["docs"]));
    assert!(test.screen().contains("  MCP configuration reloaded."));
    assert_eq!(pending(&test), (None, LoadState::Ready, Screen::Browse));
}

#[test]
fn a_refresh_that_drops_the_selected_server_keeps_a_reload_and_cancels_a_sign_in() {
    let mut test = opened(&["docs", "remote"]);
    press(&mut test, b"r");
    test.deliver(UiEvent::McpMenuRefreshed {
        snapshot: servers(&["remote"]),
    });
    assert_eq!(
        pending(&test),
        (Some(1), LoadState::Loading, Screen::Browse)
    );
    test.deliver(settled(1, true, "MCP configuration reloaded.", &["remote"]));
    assert!(test.screen().contains("  MCP configuration reloaded."));
    assert_eq!(pending(&test), (None, LoadState::Ready, Screen::Browse));
    press(&mut test, b"\r\r");
    assert_eq!(
        test.sent().last(),
        Some(&menu(McpMenuRequest::Authenticate {
            generation: 2,
            server: "remote".to_owned(),
        }))
    );
    test.deliver(UiEvent::McpMenuRefreshed {
        snapshot: servers(&["docs"]),
    });
    assert_eq!(pending(&test), (None, LoadState::Ready, Screen::Browse));
}
