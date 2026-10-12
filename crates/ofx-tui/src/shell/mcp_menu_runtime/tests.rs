use ofx_contract::{
    McpMenuAddForm, McpMenuAddTransport, McpMenuAdmission, McpMenuAuthentication, McpMenuCancel,
    McpMenuCatalog, McpMenuCatalogRequest, McpMenuConnection, McpMenuCounts, McpMenuOutcome,
    McpMenuPreview, McpMenuPreviewTarget, McpMenuPrompt, McpMenuRequest, McpMenuResource,
    McpMenuServer, McpMenuSettled, McpMenuSnapshot, McpMenuSource, McpMenuStatus, McpMenuTransport,
    McpMenuTrust, UiCommand, UiEvent, UsageMenuUpdate, UsageScope,
};

use super::super::mcp_menu::{Action, Event, LoadState, Screen, Section};
use crate::shell::test_shell::TestShell;

const ROOT_HINT: &str = "↑↓ move  tab section  enter inspect  a add  r reload  c help  esc close";

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
    let outcome = if succeeded {
        McpMenuOutcome::Succeeded
    } else {
        McpMenuOutcome::Failed
    };
    UiEvent::McpMenuSettled {
        settled: McpMenuSettled {
            generation,
            outcome,
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
    let screen = press(&mut test, b"qwe");
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
        screen.contains("enter sign in  d remove  c help  esc back"),
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
            menu(McpMenuRequest::Cancel {
                generation: 1,
                reason: McpMenuCancel::Closed,
            }),
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

fn opened_with(servers: Vec<McpMenuServer>) -> TestShell {
    let mut test = TestShell::start();
    test.deliver(UiEvent::McpMenuOpened {
        snapshot: McpMenuSnapshot {
            servers,
            configuration_issues: Vec::new(),
            profile_config: None,
        },
    });
    test
}

fn outcome(generation: u64, outcome: McpMenuOutcome, feedback: &str) -> UiEvent {
    outcome_for(generation, outcome, feedback, servers(&["docs"]))
}

fn outcome_for(
    generation: u64,
    outcome: McpMenuOutcome,
    feedback: &str,
    snapshot: McpMenuSnapshot,
) -> UiEvent {
    UiEvent::McpMenuSettled {
        settled: McpMenuSettled {
            generation,
            outcome,
            feedback: feedback.to_owned(),
        },
        snapshot,
    }
}

fn requests(test: &TestShell) -> Vec<UiCommand> {
    test.sent()
        .into_iter()
        .filter(|command| command != &menu(McpMenuRequest::Refresh))
        .collect()
}

fn state(test: &TestShell) -> &super::super::mcp_menu::McpMenuState {
    &test.shell.mcp_menu.as_ref().unwrap().state
}

#[test]
fn the_add_form_takes_each_field_from_the_composer_and_saves_once_on_the_last() {
    let mut test = opened(&["docs"]);
    let screen = press(&mut test, b"a");
    assert!(screen.contains("> Name"), "{screen}");
    press(&mut test, b"fixture");
    let screen = press(&mut test, b"\r");
    assert!(test.shell.composer.is_empty());
    assert!(screen.contains("  Name                fixture"), "{screen}");
    assert!(screen.contains("> Command"), "{screen}");
    press(&mut test, b"/usr/bin/env\r");
    press(&mut test, b"\x03");
    press(&mut test, b"node  fixture.mjs\r");
    press(&mut test, b"\t\r");
    assert_eq!(state(&test).add_transport, McpMenuAddTransport::Local);
    assert_eq!(
        test.sent(),
        [menu(McpMenuRequest::Add {
            generation: 1,
            form: McpMenuAddForm {
                transport: McpMenuAddTransport::Local,
                name: "fixture".to_owned(),
                target: "/usr/bin/env".to_owned(),
                arguments: "node  fixture.mjs".to_owned(),
            },
        })]
    );
    test.deliver(outcome(
        1,
        McpMenuOutcome::Continuing,
        "Saved MCP server; reconnecting…",
    ));
    let screen = test.screen();
    assert!(
        screen.contains("  Saved MCP server; reconnecting…"),
        "{screen}"
    );
    assert_eq!(state(&test).screen, Screen::Browse);
    assert_eq!(state(&test).load_state, LoadState::Loading);
    test.deliver(outcome(
        1,
        McpMenuOutcome::Succeeded,
        "MCP configuration reloaded.",
    ));
    assert_eq!(state(&test).load_state, LoadState::Ready);
}

#[test]
fn tab_switches_the_add_form_to_http_and_escape_returns_to_the_list() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"a");
    press(&mut test, b"draft");
    let screen = press(&mut test, b"\t");
    assert!(screen.contains("  Transport           HTTP"), "{screen}");
    assert!(screen.contains("> Name"), "{screen}");
    assert!(!screen.contains("Arguments"), "{screen}");
    assert!(test.shell.composer.is_empty());
    press(&mut test, b"remote\r");
    press(&mut test, b"https://mcp.example/mcp\r");
    assert_eq!(
        test.sent(),
        [menu(McpMenuRequest::Add {
            generation: 1,
            form: McpMenuAddForm {
                transport: McpMenuAddTransport::Http,
                name: "remote".to_owned(),
                target: "https://mcp.example/mcp".to_owned(),
                arguments: String::new(),
            },
        })]
    );
    test.deliver(outcome(
        1,
        McpMenuOutcome::Failed,
        "MCP operation failed: McpServerExists",
    ));
    assert_eq!(state(&test).screen, Screen::Add);
    press(&mut test, b"half typed");
    escape(&mut test);
    assert_eq!(state(&test).screen, Screen::Browse);
    assert!(test.shell.composer.is_empty());
}

#[test]
fn remove_and_logout_ask_for_confirmation_where_upstream_offers_them() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"d");
    assert_eq!(state(&test).screen, Screen::Browse);
    press(&mut test, b"\r");
    press(&mut test, b"l");
    assert_eq!(state(&test).screen, Screen::Details);
    let screen = press(&mut test, b"d");
    assert!(
        screen.contains("Remove this profile MCP server? press enter to confirm."),
        "{screen}"
    );
    escape(&mut test);
    assert_eq!(state(&test).screen, Screen::Browse);
    assert!(requests(&test).is_empty());
    press(&mut test, b"\r");
    press(&mut test, b"d");
    press(&mut test, b"\r\r");
    assert_eq!(
        requests(&test),
        [menu(McpMenuRequest::Remove {
            generation: 1,
            server: "docs".to_owned(),
        })]
    );
    let mut signed_in = opened_with(vec![McpMenuServer {
        transport: McpMenuTransport::Http,
        authentication: McpMenuAuthentication::Authenticated,
        ..server("remote", McpMenuTransport::Http, McpMenuStatus::Ready)
    }]);
    press(&mut signed_in, b"\r");
    press(&mut signed_in, b"l");
    signed_in.shell.mcp_menu.as_mut().unwrap().feedback = Some("earlier".to_owned());
    press(&mut signed_in, b"\r");
    assert_eq!(signed_in.shell.mcp_menu.as_ref().unwrap().feedback, None);
    assert_eq!(
        signed_in.sent(),
        [menu(McpMenuRequest::Logout {
            generation: 1,
            server: "remote".to_owned(),
        })]
    );
    signed_in.deliver(outcome(
        1,
        McpMenuOutcome::Returned,
        "Logged out of MCP server 'remote'. Run /mcp auth remote --open to sign in again.",
    ));
    assert_eq!(state(&signed_in).screen, Screen::Browse);
    assert_eq!(state(&signed_in).load_state, LoadState::Ready);
}

#[test]
fn trust_keys_approve_directly_and_confirm_the_rest() {
    let pending = McpMenuServer {
        source: McpMenuSource::Project,
        admission: Some(McpMenuAdmission::Pending),
        ..server("docs", McpMenuTransport::Stdio, McpMenuStatus::Unavailable)
    };
    let listed = McpMenuSnapshot {
        servers: vec![pending.clone()],
        configuration_issues: Vec::new(),
        profile_config: None,
    };
    let mut test = opened_with(vec![pending]);
    press(&mut test, b"\r");
    press(&mut test, b"a");
    let trust = |generation, change| menu(McpMenuRequest::Trust { generation, change });
    assert_eq!(
        test.sent(),
        [trust(1, McpMenuTrust::Approve("docs".to_owned()))]
    );
    test.deliver(outcome_for(
        1,
        McpMenuOutcome::Returned,
        "Updated project MCP trust; reconnecting…",
        listed.clone(),
    ));
    assert_eq!(state(&test).screen, Screen::Browse);
    for (keys, change) in [
        (&b"p"[..], McpMenuTrust::ApproveAll),
        (b"z", McpMenuTrust::Reset),
    ] {
        let screen = press(&mut test, keys);
        assert!(screen.contains("press enter to confirm."), "{screen}");
        press(&mut test, b"\r");
        let generation = u64::try_from(requests(&test).len()).unwrap();
        assert_eq!(requests(&test).last(), Some(&trust(generation, change)));
        test.deliver(outcome_for(
            generation,
            McpMenuOutcome::Continuing,
            "Updated project MCP trust; reconnecting…",
            listed.clone(),
        ));
        assert_eq!(state(&test).screen, Screen::Browse);
        test.deliver(outcome_for(
            generation,
            McpMenuOutcome::Succeeded,
            "done",
            listed.clone(),
        ));
    }
    press(&mut test, b"\r");
    press(&mut test, b"x");
    press(&mut test, b"\r");
    assert_eq!(
        requests(&test).last(),
        Some(&trust(4, McpMenuTrust::Reject("docs".to_owned())))
    );
}

#[test]
fn the_add_form_edits_on_ctrl_d_and_dollar_without_exiting_or_mentioning_a_skill() {
    let mut test = TestShell::start_with(|options| {
        options.skill_catalog = Some(Box::new(OneSkill));
    });
    test.deliver(UiEvent::McpMenuOpened {
        snapshot: servers(&["docs"]),
    });
    press(&mut test, b"a");
    press(&mut test, b"\x04");
    assert!(!test.shell.should_exit);
    assert_eq!(state(&test).screen, Screen::Add);
    let screen = press(&mut test, b"$rev");
    assert_eq!(test.shell.composer.text(), "$rev");
    assert!(test.shell.skills_menu.is_none(), "{screen}");
    press(&mut test, b"\x01\x04\x0e\x03");
    assert_eq!(test.shell.composer.text(), "rev");
    press(&mut test, b"\r");
    assert_eq!(state(&test).add_field_index, 1);
    assert_eq!(test.shell.mcp_menu.as_ref().unwrap().add.name, "rev");
    assert!(!test.shell.should_exit);
}

#[test]
fn a_refresh_that_drops_the_selected_server_keeps_the_add_form() {
    let mut test = opened(&["docs", "remote"]);
    press(&mut test, b"a");
    press(&mut test, b"na");
    test.deliver(UiEvent::McpMenuRefreshed {
        snapshot: servers(&["remote"]),
    });
    assert_eq!(state(&test).screen, Screen::Add);
    assert_eq!(state(&test).selected_server_index, 0);
    press(&mut test, b"pz\r");
    assert_eq!(state(&test).screen, Screen::Add);
    assert_eq!(state(&test).add_field_index, 1);
    assert_eq!(test.shell.mcp_menu.as_ref().unwrap().add.name, "napz");
    assert!(requests(&test).is_empty(), "{:?}", requests(&test));
    press(&mut test, b"/bin/true\r\r");
    assert_eq!(state(&test).pending_generation, Some(1));
    test.deliver(UiEvent::McpMenuRefreshed {
        snapshot: servers(&[]),
    });
    assert_eq!(state(&test).pending_generation, Some(1));
    test.deliver(outcome(
        1,
        McpMenuOutcome::Continuing,
        "Saved MCP server; reconnecting…",
    ));
    assert_eq!(state(&test).screen, Screen::Browse);
    assert_eq!(state(&test).load_state, LoadState::Loading);
}

#[test]
fn a_change_for_a_server_the_menu_no_longer_lists_fails_in_its_feedback_row() {
    let mut test = opened(&[]);
    test.shell
        .mcp_menu
        .as_mut()
        .unwrap()
        .state
        .apply(Event::ShowConfirmation(Action::Remove));
    let screen = press(&mut test, b"\r");
    assert!(requests(&test).is_empty(), "{:?}", requests(&test));
    assert_eq!(state(&test).load_state, LoadState::Failed);
    assert_eq!(state(&test).pending_generation, None);
    assert!(
        screen.contains("MCP operation failed: McpServerNotFound"),
        "{screen}"
    );
}

#[test]
fn a_removed_server_leaving_the_list_keeps_the_reload_that_follows() {
    let mut test = opened(&["docs", "other"]);
    press(&mut test, b"\r");
    press(&mut test, b"d");
    press(&mut test, b"\r");
    assert_eq!(
        requests(&test),
        [menu(McpMenuRequest::Remove {
            generation: 1,
            server: "docs".to_owned(),
        })]
    );
    test.deliver(outcome_for(
        1,
        McpMenuOutcome::Continuing,
        "Removed MCP server; reconnecting…",
        servers(&["docs", "other"]),
    ));
    test.deliver(UiEvent::McpMenuRefreshed {
        snapshot: servers(&["other"]),
    });
    assert_eq!(state(&test).pending_generation, Some(1));
    assert_eq!(state(&test).load_state, LoadState::Loading);
    test.deliver(outcome_for(
        1,
        McpMenuOutcome::Succeeded,
        "MCP configuration reloaded.",
        servers(&["other"]),
    ));
    let screen = test.screen();
    assert!(screen.contains("  MCP configuration reloaded."), "{screen}");
    assert_eq!(state(&test).load_state, LoadState::Ready);
}

#[test]
fn a_confirmation_stays_bound_to_the_server_it_was_opened_for() {
    let profile = |name: &str| server(name, McpMenuTransport::Stdio, McpMenuStatus::Ready);
    let signed_in = |name: &str| McpMenuServer {
        authentication: McpMenuAuthentication::Authenticated,
        ..server(name, McpMenuTransport::Http, McpMenuStatus::Ready)
    };
    let project = |name: &str| McpMenuServer {
        source: McpMenuSource::Project,
        admission: Some(McpMenuAdmission::Approved),
        ..profile(name)
    };
    let cases = [
        (
            [profile("first"), profile("second")],
            b"d",
            McpMenuRequest::Remove {
                generation: 1,
                server: "first".to_owned(),
            },
        ),
        (
            [signed_in("first"), signed_in("second")],
            b"l",
            McpMenuRequest::Logout {
                generation: 1,
                server: "first".to_owned(),
            },
        ),
        (
            [project("first"), project("second")],
            b"x",
            McpMenuRequest::Trust {
                generation: 1,
                change: McpMenuTrust::Reject("first".to_owned()),
            },
        ),
    ];
    for (servers, key, request) in cases {
        let mut test = opened_with(servers.to_vec());
        press(&mut test, b"\r");
        press(&mut test, key);
        assert_eq!(state(&test).screen, Screen::Confirm);
        press(&mut test, b"\x1b[B");
        press(&mut test, b"\n");
        press(&mut test, b"\x1b[A");
        assert_eq!(state(&test).selected_server_index, 0);
        press(&mut test, b"\r");
        assert_eq!(requests(&test), [menu(request)]);
    }
}

fn load(generation: u64, catalog: McpMenuCatalogRequest) -> UiCommand {
    menu(McpMenuRequest::LoadCatalog {
        generation,
        catalog,
    })
}

#[test]
fn tab_and_the_arrows_walk_the_catalog_tabs_of_the_selected_server() {
    let mut test = opened(&["docs", "remote"]);
    press(&mut test, b"\x1b[B");
    press(&mut test, b"\t");
    press(&mut test, b"\x1b[C");
    press(&mut test, b"\x1b[C");
    press(&mut test, b"\x1b[D");
    assert_eq!(
        requests(&test),
        [
            load(1, McpMenuCatalogRequest::Tools),
            load(2, McpMenuCatalogRequest::Resources("remote".to_owned())),
            load(3, McpMenuCatalogRequest::Prompts("remote".to_owned())),
            load(4, McpMenuCatalogRequest::Resources("remote".to_owned())),
        ]
    );
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 3,
        catalog: McpMenuCatalog::Prompts(Vec::new()),
    });
    assert_eq!(state(&test).pending_generation, Some(4));
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 4,
        catalog: McpMenuCatalog::Resources(vec![McpMenuResource {
            uri: "custom://alpha".to_owned(),
            name: "alpha".to_owned(),
            title: None,
            description: None,
            template: false,
        }]),
    });
    let screen = test.screen();
    assert!(
        screen.contains("MCP 2  Servers  Tools  [Resources]  Prompts"),
        "{screen}"
    );
    assert!(screen.contains("  Server              remote"), "{screen}");
    assert!(screen.contains("custom://alpha"), "{screen}");
    assert!(
        screen.contains("tab section     enter open     / filter     esc back"),
        "{screen}"
    );
    press(&mut test, b"arpz");
    assert_eq!(requests(&test).len(), 4);
    assert_eq!(state(&test).screen, Screen::Browse);
    press(&mut test, b"\x1b[D\x1b[D");
    let screen = test.screen();
    assert!(screen.contains(ROOT_HINT), "{screen}");
    assert_eq!(state(&test).selected_index, 1);
}

#[test]
fn the_loading_row_waits_for_its_grace_and_a_menu_without_servers_loads_nothing() {
    let mut test = opened(&["docs"]);
    let screen = press(&mut test, b"\t");
    assert!(!screen.contains("Loading MCP catalog"), "{screen}");
    test.advance(199);
    refresh_now(&mut test);
    assert!(!test.screen().contains("Loading MCP catalog"));
    test.advance(1);
    refresh_now(&mut test);
    assert!(test.screen().contains("  Loading MCP catalog…"));
    let mut empty = opened(&[]);
    press(&mut empty, b"\t\t");
    assert_eq!(requests(&empty), [load(1, McpMenuCatalogRequest::Tools)]);
    let screen = empty.screen();
    assert!(screen.contains("  No MCP resources available."), "{screen}");
    assert_eq!(
        empty.shell.mcp_menu.as_ref().unwrap().state.load_state,
        LoadState::Ready
    );
}

#[test]
fn the_filter_takes_typed_keys_until_escape_clears_it() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\t");
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 1,
        catalog: McpMenuCatalog::Tools(vec![
            "mcp_docs_echo".to_owned(),
            "mcp_docs_read".to_owned(),
        ]),
    });
    let screen = press(&mut test, b"/recz\x7f");
    assert!(screen.contains("  Filter: rec"), "{screen}");
    assert!(screen.contains("  No matching MCP items."), "{screen}");
    let screen = press(&mut test, b"\x7f\x7fea");
    assert!(screen.contains("  Filter: rea"), "{screen}");
    assert!(screen.contains("mcp_docs_read"), "{screen}");
    assert!(!screen.contains("mcp_docs_echo"), "{screen}");
    assert_eq!(requests(&test).len(), 1);
    let screen = escape(&mut test);
    assert!(!screen.contains("Filter:"), "{screen}");
    assert!(screen.contains("mcp_docs_echo"), "{screen}");
    assert!(test.shell.mcp_menu.is_some());
    escape(&mut test);
    assert!(test.shell.mcp_menu.is_none());
}

#[test]
fn a_failed_catalog_reports_in_the_feedback_row() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\t\t");
    test.deliver(outcome(
        2,
        McpMenuOutcome::Failed,
        "MCP catalog failed: McpRequestTimedOut",
    ));
    let screen = test.screen();
    assert!(
        screen.contains("  MCP catalog failed: McpRequestTimedOut"),
        "{screen}"
    );
    assert_eq!(state(&test).load_state, LoadState::Failed);
    assert_eq!(state(&test).section, Section::Resources);
}

fn cancel(generation: u64, reason: McpMenuCancel) -> UiCommand {
    menu(McpMenuRequest::Cancel { generation, reason })
}

#[test]
fn a_catalog_load_is_cancelled_when_it_can_no_longer_settle() {
    let mut test = opened(&["docs", "remote"]);
    press(&mut test, b"\t");
    press(&mut test, b"\x1b[D");
    press(&mut test, b"\t\t");
    test.deliver(UiEvent::McpMenuRefreshed {
        snapshot: servers(&["remote"]),
    });
    assert_eq!(state(&test).section, Section::Servers);
    press(&mut test, b"\t");
    escape(&mut test);
    assert!(test.shell.mcp_menu.is_none());
    assert_eq!(
        requests(&test),
        [
            load(1, McpMenuCatalogRequest::Tools),
            cancel(1, McpMenuCancel::Back),
            load(2, McpMenuCatalogRequest::Tools),
            load(3, McpMenuCatalogRequest::Resources("docs".to_owned())),
            cancel(3, McpMenuCancel::ServerRemoved),
            load(4, McpMenuCatalogRequest::Tools),
            cancel(4, McpMenuCancel::Closed),
        ]
    );
}

#[test]
fn decoded_deletions_edit_the_filter_and_raw_control_bytes_do_not() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\t");
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 1,
        catalog: McpMenuCatalog::Tools(vec!["mcp_docs_echo".to_owned()]),
    });
    press(&mut test, b"/echo\x17\x15");
    assert_eq!(state(&test).query, "echo");
    press(&mut test, b"\x1b[127;3u");
    assert_eq!(state(&test).query, "");
    press(&mut test, b"ec");
    press(&mut test, b"\x1b[127;9u");
    assert_eq!(state(&test).query, "");
    assert!(state(&test).filter_active);
}

#[test]
fn the_arrows_change_tabs_from_the_details_and_the_add_form() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\r");
    assert_eq!(state(&test).screen, Screen::Details);
    press(&mut test, b"\x1b[C");
    assert_eq!(state(&test).section, Section::Tools);
    assert_eq!(state(&test).screen, Screen::Browse);
    press(&mut test, b"\x1b[D");
    press(&mut test, b"a");
    assert_eq!(state(&test).screen, Screen::Add);
    press(&mut test, b"\x1b[D");
    assert_eq!(state(&test).section, Section::Prompts);
    assert_eq!(state(&test).screen, Screen::Browse);
    assert_eq!(
        requests(&test),
        [
            load(1, McpMenuCatalogRequest::Tools),
            cancel(1, McpMenuCancel::Back),
            load(2, McpMenuCatalogRequest::Prompts("docs".to_owned())),
        ]
    );
}

#[test]
fn a_reloaded_catalog_clamps_the_selection_to_its_filtered_rows() {
    let tools = |names: &[&str]| {
        McpMenuCatalog::Tools(names.iter().map(|name| (*name).to_owned()).collect())
    };
    let mut test = opened(&["docs"]);
    press(&mut test, b"\t");
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 1,
        catalog: tools(&["a1", "a2", "a3"]),
    });
    press(&mut test, b"\t\x1b[D");
    press(&mut test, b"/a\x1b[B\x1b[B");
    assert_eq!(state(&test).query, "a");
    assert_eq!(state(&test).selected_index, 2);
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 3,
        catalog: tools(&["a1", "b1", "b2"]),
    });
    assert_eq!(state(&test).selected_index, 0);
    let screen = test.screen();
    assert!(screen.contains("  a1"), "{screen}");
    assert!(!screen.contains("b1"), "{screen}");
}

fn preview_request(generation: u64, target: McpMenuPreviewTarget) -> UiCommand {
    menu(McpMenuRequest::LoadPreview { generation, target })
}

fn previewed(generation: u64, display: &str, insert: &str) -> UiEvent {
    UiEvent::McpMenuPreviewed {
        generation,
        preview: McpMenuPreview {
            display: display.to_owned(),
            insert: insert.to_owned(),
        },
    }
}

#[test]
fn enter_previews_a_tool_that_scrolls_and_inserts_into_the_composer() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\t");
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 1,
        catalog: McpMenuCatalog::Tools(vec!["mcp_docs_echo".to_owned()]),
    });
    press(&mut test, b"\r");
    assert_eq!(
        requests(&test).last(),
        Some(&preview_request(
            2,
            McpMenuPreviewTarget::Tool("mcp_docs_echo".to_owned())
        ))
    );
    let display = format!(
        "MCP tool schema · untrusted metadata\n\n{}",
        "word ".repeat(60)
    );
    test.deliver(previewed(2, &display, "{\"type\":\"function\"}"));
    let screen = test.screen();
    assert!(
        screen.contains("  MCP tool schema · untrusted metadata"),
        "{screen}"
    );
    assert!(
        screen.contains("↑↓ scroll     i insert     esc back"),
        "{screen}"
    );
    press(&mut test, b"\x1b[B");
    assert_eq!(state(&test).window_start, 1);
    press(&mut test, b"\x1b[A\x1b[A");
    assert_eq!(state(&test).window_start, 0);
    press(&mut test, b"i");
    assert!(test.shell.mcp_menu.is_none());
    assert_eq!(test.shell.composer.text(), "{\"type\":\"function\"}");
}

#[test]
fn enter_previews_resources_and_prompts_of_the_selected_server_but_not_templates() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\t\t");
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 2,
        catalog: McpMenuCatalog::Resources(vec![
            McpMenuResource {
                uri: "custom://alpha".to_owned(),
                name: "alpha".to_owned(),
                title: None,
                description: None,
                template: false,
            },
            McpMenuResource {
                uri: "custom://project/{path}".to_owned(),
                name: "project".to_owned(),
                title: None,
                description: None,
                template: true,
            },
        ]),
    });
    press(&mut test, b"\x1b[B\r");
    assert_eq!(requests(&test).len(), 2);
    press(&mut test, b"\x1b[A\r");
    assert_eq!(
        requests(&test).last(),
        Some(&preview_request(
            3,
            McpMenuPreviewTarget::Resource {
                server: "docs".to_owned(),
                uri: "custom://alpha".to_owned(),
            }
        ))
    );
    test.deliver(outcome(
        3,
        McpMenuOutcome::Failed,
        "MCP preview failed: McpResourceNotFound",
    ));
    let screen = test.screen();
    assert!(
        screen.contains("  MCP preview failed: McpResourceNotFound"),
        "{screen}"
    );
    assert!(!screen.contains("custom://alpha"), "{screen}");
    press(&mut test, b"\t");
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 4,
        catalog: McpMenuCatalog::Prompts(vec![McpMenuPrompt {
            name: "review".to_owned(),
            title: Some("Review".to_owned()),
            description: None,
        }]),
    });
    press(&mut test, b"\r");
    assert_eq!(
        requests(&test).last(),
        Some(&preview_request(
            5,
            McpMenuPreviewTarget::Prompt {
                server: "docs".to_owned(),
                name: "review".to_owned(),
            }
        ))
    );
    escape(&mut test);
    assert!(test.shell.mcp_menu.is_none());
    assert_eq!(
        requests(&test).last(),
        Some(&cancel(5, McpMenuCancel::Closed))
    );
}

#[test]
fn an_insert_past_the_composer_limit_keeps_the_preview_open() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\t");
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 1,
        catalog: McpMenuCatalog::Tools(vec!["mcp_docs_echo".to_owned()]),
    });
    press(&mut test, b"\r");
    let insert = "x".repeat(crate::input::COMPOSER_INPUT_LIMIT_BYTES + 1);
    test.deliver(previewed(
        2,
        "MCP tool schema · untrusted metadata",
        &insert,
    ));
    let screen = press(&mut test, b"i");
    assert!(test.shell.mcp_menu.is_some());
    assert_eq!(state(&test).screen, Screen::Preview);
    assert_eq!(state(&test).load_state, LoadState::Loading);
    assert!(test.shell.composer.is_empty());
    assert!(
        screen.contains("That edit exceeds the local prompt safety limit and was not applied."),
        "{screen}"
    );
}

#[test]
fn a_preview_targets_the_filtered_item_and_starts_without_old_feedback() {
    let mut test = opened(&["docs"]);
    press(&mut test, b"\t");
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 1,
        catalog: McpMenuCatalog::Tools(vec!["alpha".to_owned(), "beta".to_owned()]),
    });
    press(&mut test, b"\r");
    test.deliver(outcome(
        2,
        McpMenuOutcome::Failed,
        "MCP preview failed: McpToolNotFound",
    ));
    assert!(test.screen().contains("McpToolNotFound"));
    press(&mut test, b"/b\r");
    assert_eq!(
        requests(&test).last(),
        Some(&preview_request(
            3,
            McpMenuPreviewTarget::Tool("beta".to_owned())
        ))
    );
    let screen = test.screen();
    assert!(!screen.contains("McpToolNotFound"), "{screen}");
    test.deliver(previewed(3, "current", "current"));
    test.deliver(previewed(2, "stale", "stale"));
    let preview = test.shell.mcp_menu.as_ref().unwrap().preview.as_ref();
    assert_eq!(
        preview.map(|preview| preview.insert.as_str()),
        Some("current")
    );
}

#[test]
fn a_catalog_cached_for_one_server_is_neither_shown_nor_opened_for_another() {
    let resource = |uri: &str| McpMenuResource {
        uri: uri.to_owned(),
        name: uri.to_owned(),
        title: None,
        description: None,
        template: false,
    };
    let mut test = opened(&["first", "second"]);
    press(&mut test, b"\t\t");
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 2,
        catalog: McpMenuCatalog::Resources(vec![resource("custom://first-only")]),
    });
    assert!(test.screen().contains("custom://first-only"));
    press(&mut test, b"\x1b[D\x1b[D\x1b[B\t\t");
    assert_eq!(
        requests(&test).last(),
        Some(&load(
            5,
            McpMenuCatalogRequest::Resources("second".to_owned())
        ))
    );
    test.advance(200);
    refresh_now(&mut test);
    let screen = press(&mut test, b"\r");
    assert!(!screen.contains("custom://first-only"), "{screen}");
    assert_eq!(
        requests(&test).last(),
        Some(&load(
            5,
            McpMenuCatalogRequest::Resources("second".to_owned())
        ))
    );
    test.deliver(UiEvent::McpMenuLoaded {
        generation: 5,
        catalog: McpMenuCatalog::Resources(vec![resource("custom://second-only")]),
    });
    press(&mut test, b"\r");
    assert_eq!(
        requests(&test).last(),
        Some(&preview_request(
            6,
            McpMenuPreviewTarget::Resource {
                server: "second".to_owned(),
                uri: "custom://second-only".to_owned(),
            }
        ))
    );
}
