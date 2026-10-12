use ofx_contract::{
    McpMenuAuthentication, McpMenuConnection, McpMenuCounts, McpMenuPrompt, McpMenuResource,
    McpMenuServer, McpMenuSnapshot, McpMenuSource, McpMenuStatus, McpMenuTransport,
};

use super::*;
use crate::shell::mcp_menu::{Action, Effect, Event, McpMenuState, Screen};

static NO_CATALOGS: McpMenuCatalogs = McpMenuCatalogs {
    tools: Vec::new(),
    resources: Vec::new(),
    prompts: Vec::new(),
};

fn theme() -> Theme {
    Theme::builtin(false, true, true)
}

fn ready(name: &str) -> McpMenuServer {
    McpMenuServer {
        name: name.to_owned(),
        source: McpMenuSource::Profile,
        admission: None,
        transport: McpMenuTransport::Stdio,
        required: false,
        connection: McpMenuConnection::Ready,
        authentication: McpMenuAuthentication::Authenticated,
        status: McpMenuStatus::Ready,
        reloading: false,
        protocol_version: Some("2025-06-18".to_owned()),
        counts: McpMenuCounts {
            tools: Some(2),
            resources: Some(3),
            resource_templates: Some(1),
            prompts: Some(4),
        },
        failure: None,
    }
}

fn snapshot(servers: Vec<McpMenuServer>) -> McpMenuSnapshot {
    McpMenuSnapshot {
        servers,
        configuration_issues: Vec::new(),
        profile_config: Some("~/.config/oh-fx/mcp.json".to_owned()),
    }
}

fn frame(state: &McpMenuState, snapshot: &McpMenuSnapshot, width: usize, budget: usize) -> String {
    frame_with(state, snapshot, None, AddFields::default(), width, budget)
}

fn frame_with(
    state: &McpMenuState,
    snapshot: &McpMenuSnapshot,
    feedback: Option<&str>,
    add: AddFields<'_>,
    width: usize,
    budget: usize,
) -> String {
    let view = McpMenuView {
        state,
        snapshot,
        feedback,
        add,
        catalogs: &NO_CATALOGS,
        loading_visible: true,
        preview: None,
    };
    let rows = mcp_menu_rows(&theme(), view, width, menu_row_count(view, budget, width));
    rows.iter()
        .map(|row| {
            assert!(
                row.width() <= width,
                "{:?} is wider than {width}",
                row.text()
            );
            row.text()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn hint(state: &McpMenuState, snapshot: &McpMenuSnapshot, width: usize) -> String {
    let view = McpMenuView {
        state,
        snapshot,
        feedback: None,
        add: AddFields::default(),
        catalogs: &NO_CATALOGS,
        loading_visible: true,
        preview: None,
    };
    mcp_menu_hint_row(&theme(), view, width, false).text()
}

#[test]
fn the_empty_server_state_is_compact_and_actionable() {
    let state = McpMenuState::open(0);
    let empty = snapshot(Vec::new());
    let view = McpMenuView {
        state: &state,
        snapshot: &empty,
        feedback: None,
        add: AddFields::default(),
        catalogs: &NO_CATALOGS,
        loading_visible: true,
        preview: None,
    };
    assert_eq!(menu_row_count(view, 20, 80), 3);
    let rows = frame(&state, &empty, 100, 20);
    let lines: Vec<&str> = rows.lines().collect();
    assert_eq!(lines[0], "MCP 0  [Servers]  Tools  Resources  Prompts");
    assert_eq!(lines[2], "  No MCP servers configured.");
    let issues = McpMenuSnapshot {
        configuration_issues: vec![".mcp.json server 'broken' is invalid".to_owned()],
        ..empty
    };
    assert!(
        frame(&state, &issues, 100, 20)
            .contains("No MCP servers available; configuration errors need attention.")
    );
}

#[test]
fn rows_stay_within_a_narrow_terminal() {
    let servers = snapshot(vec![ready("a-very-long-server-name-that-wraps")]);
    let mut state = McpMenuState::open(1);
    for width in [1, 4, 5, 24, 55, 56, 79, 80] {
        for budget in [1, 2, 3, 4, 12] {
            frame(&state, &servers, width, budget);
        }
    }
    state.apply(Event::ShowDetails);
    frame(&state, &servers, 24, 4);
    state.apply(Event::ShowInfo);
    frame(&state, &servers, 24, 12);
}

#[test]
fn server_text_cannot_inject_terminal_control_sequences_or_break_rows() {
    let state = McpMenuState::open(2);
    let hostile = snapshot(vec![
        ready("bad\u{1b}]0;owned\u{7}"),
        ready("multi\nline\tname"),
    ]);
    let rows = frame(&state, &hostile, 100, 12);
    assert!(!rows.contains("\u{1b}]0;owned"), "{rows}");
    assert!(rows.contains("bad\\x1b]0;owned\\x07"), "{rows}");
    assert!(rows.contains("multi line name"), "{rows}");
    assert_eq!(rows.lines().count(), 4);
}

#[test]
fn the_selection_stays_visible_with_one_body_row() {
    let servers = snapshot(
        (0..6)
            .map(|index| ready(&format!("server-{index}")))
            .collect(),
    );
    let mut state = McpMenuState::open(6);
    state.selected_index = 5;
    state.selected_server_index = 5;
    let view = McpMenuView {
        state: &state,
        snapshot: &servers,
        feedback: None,
        add: AddFields::default(),
        catalogs: &NO_CATALOGS,
        loading_visible: true,
        preview: None,
    };
    assert_eq!(menu_row_count(view, 3, 40), 3);
    let rows = frame(&state, &servers, 40, 3);
    assert_eq!(rows.lines().nth(2), Some("  server-5"));
}

#[test]
fn the_server_list_renders_its_upstream_rows() {
    let fixture = ready("fixture");
    let state = McpMenuState::open(1);
    let servers = snapshot(vec![fixture.clone()]);
    let browse = frame(&state, &servers, 100, MAX_INLINE_ROWS);
    assert!(browse.contains("MCP 1  [Servers]"), "{browse}");
    assert!(
        browse.contains("  fixture               Ready                 stdio · Profile"),
        "{browse}"
    );

    let issues = McpMenuSnapshot {
        configuration_issues: vec![
            ".mcp.json server 'broken' is missing MISSING_COMMAND in field command".to_owned(),
            ".mcp.json server 'other' is invalid".to_owned(),
        ],
        ..servers.clone()
    };
    let listed = frame(&state, &issues, 100, MAX_INLINE_ROWS);
    assert!(
        listed.contains("2 project MCP configuration errors. Run oh-fx mcp list for details. .mcp.json server 'broken'"),
        "{listed}"
    );

    let needs_auth = McpMenuServer {
        transport: McpMenuTransport::Http,
        authentication: McpMenuAuthentication::Required,
        status: McpMenuStatus::NeedsAuth,
        ..fixture.clone()
    };
    let auth = frame(&state, &snapshot(vec![needs_auth]), 100, MAX_INLINE_ROWS);
    assert!(
        auth.contains("Needs authentication  HTTP · Profile"),
        "{auth}"
    );

    let reloading = McpMenuServer {
        reloading: true,
        ..fixture.clone()
    };
    let pending = McpMenuServer {
        name: "other".to_owned(),
        source: McpMenuSource::Project,
        admission: Some(McpMenuAdmission::Pending),
        ..fixture.clone()
    };
    let mixed = frame(
        &McpMenuState::open(2),
        &snapshot(vec![reloading, pending]),
        100,
        MAX_INLINE_ROWS,
    );
    assert!(mixed.contains("fixture               Reloading"), "{mixed}");
    assert!(
        mixed.contains("other                 Pending trust         stdio · Project"),
        "{mixed}"
    );
}

#[test]
fn the_details_and_help_screens_render_their_upstream_facts() {
    let fixture = ready("fixture");
    let mut state = McpMenuState::open(1);
    let servers = snapshot(vec![fixture.clone()]);
    state.apply(Event::ShowDetails);
    let details = frame(&state, &servers, 100, MAX_INLINE_ROWS);
    for fact in [
        "  Server              fixture",
        "  State               Ready",
        "  Source              Profile · ~/.config/oh-fx/mcp.json",
        "  Transport           stdio",
        "  Policy              optional",
        "  Protocol            2025-06-18",
        "  Capabilities        tools=2 resources=3 templates=1 prompts=4",
    ] {
        assert!(details.contains(fact), "{fact:?} in {details}");
    }
    assert!(!details.contains("Error"), "{details}");
    let failed = McpMenuServer {
        status: McpMenuStatus::Failed,
        failure: Some("boom".to_owned()),
        counts: McpMenuCounts::default(),
        protocol_version: None,
        required: true,
        source: McpMenuSource::Project,
        ..fixture
    };
    let broken = frame(&state, &snapshot(vec![failed]), 100, MAX_INLINE_ROWS);
    for fact in [
        "  Source              Project · .mcp.json",
        "  Policy              required",
        "  Protocol            unavailable",
        "  Capabilities        tools=? resources=? templates=? prompts=?",
        "  Error               boom",
    ] {
        assert!(broken.contains(fact), "{fact:?} in {broken}");
    }
    let gone = frame(&state, &snapshot(Vec::new()), 100, MAX_INLINE_ROWS);
    assert!(
        gone.contains("MCP server is no longer available."),
        "{gone}"
    );

    state.apply(Event::ShowInfo);
    let info = frame(&state, &servers, 100, MAX_INLINE_ROWS);
    for line in [
        "  Profile config      ~/.config/oh-fx/mcp.json",
        "  Project config      <workspace>/.mcp.json",
        "  servers: a add · r reload · enter inspect",
        "  project trust: p approve all · z reset",
        "  details: enter sign in · l logout · d remove",
        "  project details: a approve · x reject",
        "  catalogs: / filter · enter open · i insert preview",
    ] {
        assert!(info.contains(line), "{line:?} in {info}");
    }
}

#[test]
fn feedback_takes_the_second_header_row_and_warns_after_a_failure() {
    let mut state = McpMenuState::open(1);
    let servers = snapshot(vec![ready("fixture")]);
    let shown = frame_with(
        &state,
        &servers,
        Some("MCP configuration reloaded."),
        AddFields::default(),
        80,
        12,
    );
    assert_eq!(shown.lines().nth(1), Some("  MCP configuration reloaded."));
    let failure = Some("MCP reload failed: LockBusy");
    let feedback_paint = |state: &McpMenuState| {
        let view = McpMenuView {
            state,
            snapshot: &servers,
            feedback: failure,
            add: AddFields::default(),
            catalogs: &NO_CATALOGS,
            loading_visible: true,
            preview: None,
        };
        mcp_menu_rows(&theme(), view, 80, 3)[1].segments()[1].paint
    };
    assert_eq!(feedback_paint(&state), theme().dim);
    state.apply(Event::RequestAction(Action::Refresh));
    state.apply(Event::EffectFailed(1));
    assert_eq!(feedback_paint(&state), theme().warning);
}

#[test]
fn hints_fit_the_width_and_name_only_available_actions() {
    let mut state = McpMenuState::open(1);
    let remote = McpMenuServer {
        transport: McpMenuTransport::Http,
        connection: McpMenuConnection::Disconnected,
        authentication: McpMenuAuthentication::None,
        status: McpMenuStatus::Unavailable,
        ..ready("remote")
    };
    let servers = snapshot(vec![remote]);
    assert_eq!(
        hint(&state, &servers, 100),
        "↑↓ move  tab section  enter inspect  a add  r reload  c help  esc close"
    );
    assert_eq!(
        hint(&state, &servers, 60),
        "↑↓ move  tab section  enter  a add  r reload  c help  esc"
    );
    assert_eq!(hint(&state, &servers, 20), "tab enter a r c esc");
    state.apply(Event::ShowDetails);
    assert_eq!(
        hint(&state, &servers, 100),
        "enter sign in  d remove  c help  esc back"
    );
    let signed_in = snapshot(vec![McpMenuServer {
        transport: McpMenuTransport::Http,
        ..ready("signed")
    }]);
    assert_eq!(
        hint(&state, &signed_in, 100),
        "d remove  l logout  c help  esc back"
    );
    let project = snapshot(vec![McpMenuServer {
        source: McpMenuSource::Project,
        admission: Some(McpMenuAdmission::Pending),
        ..ready("project")
    }]);
    assert_eq!(
        hint(&state, &project, 100),
        "a approve  x reject  c help  esc back"
    );
    assert_eq!(hint(&state, &servers, 20), "c help  esc back");
    assert_eq!(hint(&state, &servers, 10), "c esc");
    state.apply(Event::ShowInfo);
    assert_eq!(hint(&state, &servers, 100), "esc back");
    let view = McpMenuView {
        state: &state,
        snapshot: &servers,
        feedback: None,
        add: AddFields::default(),
        catalogs: &NO_CATALOGS,
        loading_visible: true,
        preview: None,
    };
    assert_eq!(
        mcp_menu_hint_row(&theme(), view, 100, true).text(),
        "press ctrl+c again to exit"
    );
}

#[test]
fn the_add_form_shows_its_transport_fields_and_the_draft() {
    let mut state = McpMenuState::open(0);
    state.apply(Event::ShowAdd);
    let empty = snapshot(Vec::new());
    let add = AddFields {
        name: "",
        target: "/usr/bin/env",
        arguments: "node fixture.mjs",
        draft: "fixture",
    };
    let local = frame_with(&state, &empty, None, add, 100, MAX_INLINE_ROWS);
    for line in [
        "  Transport           Local",
        "  > Name              fixture",
        "  Command             /usr/bin/env",
        "  Arguments           node fixture.mjs",
        "  Scope               Profile",
    ] {
        assert!(local.contains(line), "{line:?} in {local}");
    }
    assert_eq!(
        hint(&state, &empty, 100),
        "type field     enter next/save     tab transport     esc cancel"
    );
    state.apply(Event::CycleAddTransport);
    state.apply(Event::MoveAddField {
        delta: 1,
        field_count: 2,
    });
    let http = frame_with(
        &state,
        &empty,
        None,
        AddFields {
            name: "remote",
            target: "",
            arguments: "",
            draft: "https://mcp.example/mcp",
        },
        100,
        MAX_INLINE_ROWS,
    );
    for line in [
        "  Transport           HTTP",
        "  Name                remote",
        "  > URL               https://mcp.example/mcp",
        "  Scope               Profile",
    ] {
        assert!(http.contains(line), "{line:?} in {http}");
    }
    assert!(!http.contains("Arguments"), "{http}");
    assert_eq!(http.lines().count(), 6);
}

#[test]
fn a_confirmation_names_the_destructive_action() {
    let servers = snapshot(vec![ready("fixture")]);
    for (action, text) in [
        (
            Action::Remove,
            "Remove this profile MCP server? press enter to confirm.",
        ),
        (
            Action::Logout,
            "Log out of this MCP server? press enter to confirm.",
        ),
        (
            Action::TrustReject,
            "Reject this project MCP server? press enter to confirm.",
        ),
        (
            Action::TrustApproveAll,
            "Approve all pending project MCP servers? press enter to confirm.",
        ),
        (
            Action::TrustReset,
            "Reset all project MCP choices? press enter to confirm.",
        ),
    ] {
        let mut state = McpMenuState::open(1);
        state.apply(Event::ShowConfirmation(action));
        let shown = frame(&state, &servers, 100, MAX_INLINE_ROWS);
        assert_eq!(shown.lines().nth(2), Some(text), "{shown}");
        assert_eq!(shown.lines().count(), 3);
        assert_eq!(hint(&state, &servers, 100), "enter confirm     esc cancel");
        assert_eq!(hint(&state, &servers, 20), "enter confirm  esc");
    }
}

fn catalog_frame(
    state: &McpMenuState,
    catalogs: &McpMenuCatalogs,
    feedback: Option<&str>,
    width: usize,
    budget: usize,
) -> Vec<String> {
    let servers = snapshot(vec![ready("docs")]);
    let view = McpMenuView {
        state,
        snapshot: &servers,
        feedback,
        add: AddFields::default(),
        catalogs,
        loading_visible: true,
        preview: None,
    };
    mcp_menu_rows(&theme(), view, width, menu_row_count(view, budget, width))
        .iter()
        .map(|row| {
            assert!(
                row.width() <= width,
                "{:?} is wider than {width}",
                row.text()
            );
            row.text()
        })
        .collect()
}

fn resource(
    uri: &str,
    title: Option<&str>,
    description: Option<&str>,
    template: bool,
) -> McpMenuResource {
    McpMenuResource {
        uri: uri.to_owned(),
        name: uri.rsplit('/').next().unwrap_or_default().to_owned(),
        title: title.map(str::to_owned),
        description: description.map(str::to_owned),
        template,
    }
}

fn catalogs() -> McpMenuCatalogs {
    McpMenuCatalogs {
        tools: vec!["mcp_docs_echo".to_owned()],
        resources: vec![
            resource("custom://alpha", Some("Alpha"), Some("unused"), false),
            resource("custom://beta", None, Some("Beta file"), false),
            resource("custom://gamma", None, None, false),
            resource("custom://project/{path}", None, None, true),
        ],
        prompts: vec![
            McpMenuPrompt {
                name: "review".to_owned(),
                title: Some("Review".to_owned()),
                description: Some("Review code".to_owned()),
            },
            McpMenuPrompt {
                name: "plain".to_owned(),
                title: None,
                description: None,
            },
        ],
    }
}

fn loaded(state: &mut McpMenuState, delta: isize, item_count: usize) {
    let Some(Effect::LoadCatalog { generation, .. }) = state.apply(Event::CycleSection(delta))
    else {
        panic!("expected a catalog load");
    };
    state.apply(Event::CatalogLoaded {
        generation,
        item_count,
    });
}

fn row(identity: &str, description: &str) -> String {
    format!("  {identity:<36}{description}")
}

#[test]
fn catalog_tabs_show_each_sections_identity_and_description() {
    let catalogs = catalogs();
    let mut state = McpMenuState::open(1);
    loaded(&mut state, 1, 1);
    assert_eq!(
        catalog_frame(&state, &catalogs, None, 80, 12),
        [
            "MCP 1  Servers  [Tools]  Resources  Prompts".to_owned(),
            "  All available servers".to_owned(),
            row("mcp_docs_echo", "Tool"),
        ]
    );
    assert_eq!(
        catalog_frame(&state, &catalogs, None, 40, 12)[2],
        "  mcp_docs_echo"
    );
    loaded(&mut state, 1, 4);
    assert_eq!(
        catalog_frame(&state, &catalogs, None, 80, 12),
        [
            "MCP 1  Servers  Tools  [Resources]  Prompts".to_owned(),
            "  Server              docs".to_owned(),
            row("custom://alpha", "Alpha"),
            row("custom://beta", "Beta file"),
            row("custom://gamma", "gamma"),
            row("custom://project/{path}", "Template"),
        ]
    );
    loaded(&mut state, 1, 2);
    let prompts = catalog_frame(&state, &catalogs, None, 80, 12);
    assert_eq!(
        prompts[2..],
        [row("Review", "Review code"), row("plain", "Prompt")]
    );
    assert_eq!(
        catalog_frame(&state, &catalogs, None, 30, 12)[0],
        "MCP 1  [Prompts]"
    );
}

#[test]
fn a_filter_narrows_the_catalog_and_names_an_empty_match() {
    let catalogs = catalogs();
    let mut state = McpMenuState::open(1);
    state.apply(Event::CycleSection(1));
    loaded(&mut state, 1, 4);
    state.apply(Event::BeginFilter);
    for byte in b"PROJ" {
        state.apply(Event::AppendFilterByte(*byte));
    }
    assert_eq!(
        catalog_frame(&state, &catalogs, None, 80, 12)[1..],
        [
            "  Filter: PROJ".to_owned(),
            row("custom://project/{path}", "Template"),
        ]
    );
    state.apply(Event::AppendFilterByte(b'x'));
    assert_eq!(
        catalog_frame(&state, &catalogs, None, 80, 12)[2],
        "  No matching MCP items."
    );
    let servers = snapshot(vec![ready("docs")]);
    let view = McpMenuView {
        state: &state,
        snapshot: &servers,
        feedback: None,
        add: AddFields::default(),
        catalogs: &catalogs,
        loading_visible: true,
        preview: None,
    };
    assert_eq!(view.item_count(), 0);
    assert_eq!(
        mcp_menu_hint_row(&theme(), view, 80, false).text(),
        "↑↓ navigate     tab section     enter open     / filter     esc back"
    );
    assert_eq!(
        mcp_menu_hint_row(&theme(), view, 50, false).text(),
        "↑↓ move  tab section  enter  / filter  esc"
    );
}

#[test]
fn empty_loading_and_failed_catalogs_say_so() {
    let empty = McpMenuCatalogs::default();
    let mut state = McpMenuState::open(1);
    for text in [
        "No MCP tools available.",
        "No MCP resources available.",
        "No MCP prompts available.",
    ] {
        loaded(&mut state, 1, 0);
        assert_eq!(
            catalog_frame(&state, &empty, None, 80, 12)[2],
            format!("  {text}")
        );
    }
    let Some(Effect::LoadCatalog { generation, .. }) = state.apply(Event::CycleSection(-1)) else {
        panic!("expected a catalog load");
    };
    assert_eq!(
        catalog_frame(&state, &empty, None, 80, 12)[2],
        "  Loading MCP catalog…"
    );
    let servers = snapshot(vec![ready("docs")]);
    let hidden = McpMenuView {
        state: &state,
        snapshot: &servers,
        feedback: None,
        add: AddFields::default(),
        catalogs: &empty,
        loading_visible: false,
        preview: None,
    };
    let rows = mcp_menu_rows(&theme(), hidden, 80, 3);
    assert_eq!(rows[2].text(), "");
    state.apply(Event::EffectFailed(generation));
    let failure = "MCP catalog failed: McpRequestTimedOut";
    assert_eq!(
        catalog_frame(&state, &empty, Some(failure), 80, 12)[1..],
        [format!("  {failure}"), String::new()]
    );
    assert_eq!(
        catalog_frame(&state, &empty, Some(failure), 80, 2),
        [format!("  {failure}")]
    );
}

#[test]
fn catalog_text_cannot_inject_terminal_control_sequences_or_break_rows() {
    let hostile = McpMenuCatalogs {
        tools: vec![
            "mcp_bad\u{1b}]0;owned\u{7}".to_owned(),
            "mcp_multi\nline\tname".to_owned(),
        ],
        ..McpMenuCatalogs::default()
    };
    let mut state = McpMenuState::open(1);
    loaded(&mut state, 1, 2);
    let rows = catalog_frame(&state, &hostile, None, 100, 12).join("\n");
    assert!(!rows.contains("\u{1b}]0;owned"), "{rows}");
    assert!(rows.contains("mcp_bad\\x1b]0;owned\\x07"), "{rows}");
    assert!(rows.contains("mcp_multi line name"), "{rows}");
}

#[test]
fn the_catalog_selection_stays_visible_with_one_body_row() {
    let tools = McpMenuCatalogs {
        tools: (0..6).map(|index| format!("tool-{index}")).collect(),
        ..McpMenuCatalogs::default()
    };
    let mut state = McpMenuState::open(1);
    loaded(&mut state, 1, 6);
    state.selected_index = 5;
    let rows = catalog_frame(&state, &tools, None, 40, 3);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2], "  tool-5");
}

fn preview_frame(preview: &str, window_start: usize, width: usize, budget: usize) -> Vec<String> {
    let servers = snapshot(vec![ready("docs")]);
    let mut state = McpMenuState::open(1);
    loaded(&mut state, 1, 1);
    let Some(Effect::LoadPreview { generation }) = state.apply(Event::BeginPreview) else {
        panic!("expected a preview load");
    };
    state.apply(Event::PreviewLoaded(generation));
    state.window_start = window_start;
    let view = McpMenuView {
        state: &state,
        snapshot: &servers,
        feedback: None,
        add: AddFields::default(),
        catalogs: &NO_CATALOGS,
        loading_visible: true,
        preview: Some(preview),
    };
    mcp_menu_rows(&theme(), view, width, menu_row_count(view, budget, width))
        .iter()
        .map(|row| {
            assert!(
                row.width() <= width,
                "{:?} is wider than {width}",
                row.text()
            );
            row.text()
        })
        .collect()
}

#[test]
fn a_preview_wraps_long_lines_and_keeps_fitting_words_whole() {
    assert_eq!(
        preview_frame("0123456789abcdefghijkl", 0, 12, 12)[2..],
        ["  0123456789", "  abcdefghij", "  kl"]
    );
    assert_eq!(
        preview_frame("alpha documentation beta", 0, 16, 12)[2..],
        ["  alpha", "  documentation", "  beta"]
    );
    assert_eq!(preview_frame("界界界界", 0, 5, 12).len(), 6);
    assert_eq!(preview_visual_row_count(Some("one\n\ntwo"), 40), 3);
    assert_eq!(preview_visual_row_count(None, 40), 1);
}

#[test]
fn a_preview_renders_from_its_window_with_the_preview_hints() {
    let text = "0000000000111111111122222222223333333333";
    assert_eq!(
        preview_frame(text, 2, 12, 4)[2..],
        ["  2222222222", "  3333333333"]
    );
    assert_eq!(
        preview_frame(text, 9, 12, 4)[2..],
        ["  2222222222", "  3333333333"]
    );
    let servers = snapshot(vec![ready("docs")]);
    let mut state = McpMenuState::open(1);
    state.screen = Screen::Preview;
    let view = McpMenuView {
        state: &state,
        snapshot: &servers,
        feedback: None,
        add: AddFields::default(),
        catalogs: &NO_CATALOGS,
        loading_visible: true,
        preview: Some(text),
    };
    assert_eq!(
        mcp_menu_hint_row(&theme(), view, 80, false).text(),
        "↑↓ scroll     i insert     esc back"
    );
    assert_eq!(
        mcp_menu_hint_row(&theme(), view, 30, false).text(),
        "↑↓ scroll  i insert  esc"
    );
}

#[test]
fn a_preview_screen_without_content_says_it_is_loading() {
    let servers = snapshot(vec![ready("docs")]);
    let mut state = McpMenuState::open(1);
    state.screen = Screen::Preview;
    let view = McpMenuView {
        state: &state,
        snapshot: &servers,
        feedback: None,
        add: AddFields::default(),
        catalogs: &NO_CATALOGS,
        loading_visible: true,
        preview: None,
    };
    let rows = mcp_menu_rows(&theme(), view, 80, menu_row_count(view, 12, 80));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].text(), "  Loading MCP content…");
}
