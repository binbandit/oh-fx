use ofx_contract::{McpMenuCounts, McpMenuSource, McpMenuStatus};

use super::*;

fn server(name: &str) -> McpMenuServer {
    McpMenuServer {
        name: name.to_owned(),
        source: McpMenuSource::Profile,
        admission: None,
        transport: McpMenuTransport::Http,
        required: false,
        connection: McpMenuConnection::Disconnected,
        authentication: McpMenuAuthentication::None,
        status: McpMenuStatus::Unavailable,
        reloading: false,
        protocol_version: None,
        counts: McpMenuCounts::default(),
        failure: None,
    }
}

fn moved(delta: isize, item_count: usize, visible_count: usize) -> Event {
    Event::Move(Move {
        delta,
        item_count,
        visible_count,
    })
}

#[test]
fn sign_in_is_available_for_disconnected_remote_servers() {
    let mut fixture = server("fixture");
    let cases = [
        (
            McpMenuConnection::Disconnected,
            McpMenuAuthentication::None,
            true,
        ),
        (
            McpMenuConnection::Disconnected,
            McpMenuAuthentication::Configured,
            true,
        ),
        (
            McpMenuConnection::Disconnected,
            McpMenuAuthentication::Required,
            true,
        ),
        (
            McpMenuConnection::Disconnected,
            McpMenuAuthentication::Authenticated,
            false,
        ),
        (McpMenuConnection::Ready, McpMenuAuthentication::None, false),
        (
            McpMenuConnection::Failed,
            McpMenuAuthentication::None,
            false,
        ),
        (
            McpMenuConnection::Failed,
            McpMenuAuthentication::Required,
            true,
        ),
        (
            McpMenuConnection::Connecting,
            McpMenuAuthentication::Required,
            false,
        ),
        (
            McpMenuConnection::Disabled,
            McpMenuAuthentication::None,
            false,
        ),
    ];
    for (connection, authentication, expected) in cases {
        fixture.connection = connection;
        fixture.authentication = authentication;
        assert_eq!(
            server_action_available(Action::Authenticate, &fixture),
            expected,
            "{connection:?} {authentication:?}"
        );
    }
    fixture.transport = McpMenuTransport::Stdio;
    fixture.connection = McpMenuConnection::Disconnected;
    fixture.authentication = McpMenuAuthentication::Required;
    assert!(!server_action_available(Action::Authenticate, &fixture));
    fixture.transport = McpMenuTransport::Sse;
    assert!(!server_action_available(Action::Authenticate, &fixture));
    assert!(server_action_available(Action::Refresh, &fixture));
}

#[test]
fn server_selection_stays_coherent_through_details_navigation() {
    let mut state = McpMenuState::open(3);
    state.apply(Event::ShowDetails);
    state.apply(moved(1, 3, 3));
    assert_eq!(state.selected_server_index, 1);
    state.apply(Event::Back);
    assert_eq!(state.selected_server_index, state.selected_index);
    state.apply(Event::ShowDetails);
    assert_eq!(state.selected_server_index, 1);
}

#[test]
fn the_menu_opens_ready_on_the_server_list() {
    let state = McpMenuState::open(3);
    assert_eq!(state.screen, Screen::Browse);
    assert_eq!(state.load_state, LoadState::Ready);
    assert_eq!(state.pending_generation, None);
    assert_eq!(state.selected_index, 0);
    let empty = McpMenuState::open(0);
    assert_eq!((empty.selected_index, empty.window_start), (0, 0));
}

#[test]
fn navigation_wraps_and_keeps_the_selection_visible() {
    let mut state = McpMenuState::open(5);
    state.apply(moved(-1, 5, 2));
    assert_eq!(state.selected_index, 4);
    assert_eq!(state.window_start, 3);
    state.apply(moved(1, 5, 2));
    assert_eq!(state.selected_index, 0);
    assert_eq!(state.window_start, 0);
    state.apply(moved(0, 5, 2));
    state.apply(moved(1, 0, 2));
    assert_eq!(state.selected_index, 0);
}

#[test]
fn actions_carry_fresh_generations_and_only_their_own_completion_settles_them() {
    let mut state = McpMenuState::open(1);
    let refresh = state.apply(Event::RequestAction(Action::Refresh));
    assert_eq!(
        refresh,
        Some(Effect::Action {
            generation: 1,
            action: Action::Refresh
        })
    );
    assert_eq!(state.load_state, LoadState::Loading);
    let before = state.clone();
    state.apply(Event::ActionSucceeded(99));
    state.apply(Event::EffectFailed(99));
    assert_eq!(state, before);
    state.apply(Event::EffectFailed(1));
    assert_eq!(state.load_state, LoadState::Failed);
    assert_eq!(state.pending_generation, None);
    let sign_in = state.apply(Event::RequestAction(Action::Authenticate));
    assert_eq!(
        sign_in,
        Some(Effect::Action {
            generation: 2,
            action: Action::Authenticate
        })
    );
    state.apply(Event::ActionSucceeded(2));
    assert_eq!(state.load_state, LoadState::Ready);
    state.apply(Event::ActionSucceeded(2));
    assert_eq!(state.load_state, LoadState::Ready);
}

#[test]
fn generations_skip_zero_when_they_wrap() {
    let mut state = McpMenuState::open(1);
    state.next_generation = u64::MAX;
    state.apply(Event::RequestAction(Action::Refresh));
    assert_eq!(state.pending_generation, Some(u64::MAX));
    let next = state.apply(Event::RequestAction(Action::Refresh));
    assert_eq!(
        next,
        Some(Effect::Action {
            generation: 1,
            action: Action::Refresh
        })
    );
}

#[test]
fn back_leaves_a_screen_and_closes_from_the_server_list() {
    let mut state = McpMenuState::open(2);
    state.apply(Event::ShowInfo);
    assert_eq!(state.screen, Screen::Info);
    state.apply(Event::RequestAction(Action::Refresh));
    assert_eq!(state.apply(Event::Back), Some(Effect::Cancel(1)));
    assert_eq!(state.screen, Screen::Browse);
    assert_eq!(state.load_state, LoadState::Cancelled);
    assert_eq!(state.pending_generation, None);
    state.apply(Event::ActionSucceeded(1));
    assert_eq!(state.load_state, LoadState::Cancelled);
    assert_eq!(state.apply(Event::Back), Some(Effect::Close));
}

#[test]
fn destructive_actions_require_a_matching_confirmation() {
    let mut state = McpMenuState::open(1);
    assert_eq!(state.apply(Event::RequestAction(Action::Remove)), None);
    state.apply(Event::ShowConfirmation(Action::Remove));
    assert_eq!(state.screen, Screen::Confirm);
    assert_eq!(state.apply(Event::RequestAction(Action::Logout)), None);
    assert_eq!(
        state.apply(Event::RequestAction(Action::Remove)),
        Some(Effect::Action {
            generation: 1,
            action: Action::Remove
        })
    );
    state.apply(Event::ActionSucceeded(1));
    assert_eq!(state.confirmation_action, None);
    for action in [
        Action::Logout,
        Action::TrustReject,
        Action::TrustApproveAll,
        Action::TrustReset,
    ] {
        assert_eq!(
            McpMenuState::open(1).apply(Event::RequestAction(action)),
            None
        );
    }
    for action in [Action::TrustApprove, Action::AddLocal, Action::AddHttp] {
        assert!(
            McpMenuState::open(1)
                .apply(Event::RequestAction(action))
                .is_some()
        );
    }
}

#[test]
fn add_form_transport_and_field_movement_are_pure_and_bounded() {
    let mut state = McpMenuState::open(0);
    state.apply(Event::CycleAddTransport);
    assert_eq!(state.add_transport, McpMenuAddTransport::Local);
    state.apply(Event::ShowAdd);
    assert_eq!(state.screen, Screen::Add);
    assert_eq!(state.add_transport, McpMenuAddTransport::Local);
    state.apply(Event::CycleAddTransport);
    assert_eq!(state.add_transport, McpMenuAddTransport::Http);
    state.apply(Event::MoveAddField {
        delta: -1,
        field_count: 2,
    });
    assert_eq!(state.add_field_index, 1);
    state.apply(Event::MoveAddField {
        delta: 1,
        field_count: 2,
    });
    assert_eq!(state.add_field_index, 0);
    state.apply(Event::MoveAddField {
        delta: 1,
        field_count: 0,
    });
    assert_eq!(state.add_field_index, 0);
    state.apply(Event::MoveAddField {
        delta: 1,
        field_count: 3,
    });
    state.apply(Event::CycleAddTransport);
    assert_eq!(state.add_field_index, 0);
}

#[test]
fn server_changes_follow_upstreams_availability() {
    let mut fixture = server("fixture");
    assert!(server_action_available(Action::Remove, &fixture));
    assert!(!server_action_available(Action::Logout, &fixture));
    fixture.authentication = McpMenuAuthentication::Authenticated;
    assert!(server_action_available(Action::Logout, &fixture));
    fixture.transport = McpMenuTransport::Sse;
    assert!(!server_action_available(Action::Logout, &fixture));
    assert!(!server_action_available(Action::TrustApprove, &fixture));
    assert!(!server_action_available(Action::TrustReject, &fixture));
    fixture.source = McpMenuSource::Project;
    assert!(!server_action_available(Action::Remove, &fixture));
    assert!(server_action_available(Action::TrustReject, &fixture));
    fixture.admission = Some(McpMenuAdmission::Pending);
    assert!(server_action_available(Action::TrustApprove, &fixture));
    assert!(server_action_available(Action::TrustReject, &fixture));
    fixture.admission = Some(McpMenuAdmission::Rejected);
    assert!(!server_action_available(Action::TrustApprove, &fixture));
    assert!(!server_action_available(Action::TrustReject, &fixture));
    for action in [
        Action::TrustApproveAll,
        Action::TrustReset,
        Action::AddLocal,
        Action::AddHttp,
    ] {
        assert!(server_action_available(action, &fixture));
    }
}

#[test]
fn returning_to_servers_keeps_the_selected_server_and_drops_the_confirmation() {
    let mut state = McpMenuState::open(3);
    state.apply(moved(1, 3, 3));
    state.apply(Event::ShowConfirmation(Action::Remove));
    state.return_to_servers();
    assert_eq!(state.screen, Screen::Browse);
    assert_eq!(state.selected_index, 1);
    assert_eq!(state.window_start, 0);
    assert_eq!(state.confirmation_action, None);
}

#[test]
fn a_section_change_describes_one_generated_catalog_load() {
    let mut state = McpMenuState::open(1);
    assert_eq!(
        state.apply(Event::CycleSection(1)),
        Some(Effect::LoadCatalog {
            generation: 1,
            section: Section::Tools,
        })
    );
    assert_eq!(state.section, Section::Tools);
    assert_eq!(state.load_state, LoadState::Loading);
    assert_eq!(state.pending_generation, Some(1));
    assert_eq!(
        state.apply(Event::CycleSection(-1)),
        Some(Effect::Cancel(1))
    );
    assert_eq!(state.section, Section::Servers);
    assert_eq!(state.load_state, LoadState::Ready);
    assert_eq!(
        state.apply(Event::CycleSection(-1)),
        Some(Effect::LoadCatalog {
            generation: 2,
            section: Section::Prompts,
        })
    );
}

#[test]
fn a_catalog_settles_only_on_its_own_generation() {
    let mut state = McpMenuState::open(1);
    state.apply(Event::CycleSection(1));
    let loading = state.clone();
    state.apply(Event::CatalogLoaded {
        generation: 99,
        item_count: 3,
    });
    assert_eq!(state, loading);
    state.selected_index = 7;
    state.apply(Event::CatalogLoaded {
        generation: 1,
        item_count: 3,
    });
    assert_eq!(state.load_state, LoadState::Ready);
    assert_eq!(state.screen, Screen::Browse);
    assert_eq!(state.pending_generation, None);
    assert_eq!(state.selected_index, 2);
}

#[test]
fn catalog_sections_keep_the_selected_server_and_its_screens() {
    let mut state = McpMenuState::open(3);
    state.apply(moved(1, 3, 3));
    state.apply(Event::CycleSection(1));
    state.apply(moved(1, 5, 5));
    assert_eq!(state.selected_server_index, 1);
    state.apply(Event::ShowDetails);
    state.apply(Event::ShowInfo);
    assert_eq!(state.screen, Screen::Browse);
    state.apply(Event::CycleSection(-1));
    assert_eq!(state.section, Section::Servers);
    assert_eq!(state.selected_index, 1);
}

#[test]
fn filtering_is_bounded_printable_and_resets_navigation() {
    let mut state = McpMenuState::open(1);
    state.apply(Event::BeginFilter);
    assert!(!state.filter_active);
    state.apply(Event::CycleSection(1));
    state.apply(Event::BeginFilter);
    for byte in b"query\x01\x7f" {
        state.apply(Event::AppendFilterByte(*byte));
    }
    assert_eq!(state.query, "query");
    state.selected_index = 4;
    state.window_start = 3;
    state.apply(Event::DeleteFilterByte);
    assert_eq!(state.query, "quer");
    assert_eq!((state.selected_index, state.window_start), (0, 0));
    for _ in 0..200 {
        state.apply(Event::AppendFilterByte(b'x'));
    }
    assert_eq!(state.query.len(), 128);
    state.apply(Event::ClearFilterText);
    assert!(state.query.is_empty());
    assert!(state.filter_active);
    state.apply(Event::AppendFilterByte(b'a'));
    state.apply(Event::ClearFilter);
    assert!(!state.filter_active);
    assert!(state.query.is_empty());
}

#[test]
fn a_server_leaving_during_a_catalog_returns_to_the_servers() {
    let mut state = McpMenuState::open(2);
    state.apply(moved(1, 2, 2));
    state.apply(Event::CycleSection(1));
    state.apply(Event::BeginFilter);
    state.apply(Event::AppendFilterByte(b'q'));
    state.selected_server_index = 0;
    assert_eq!(state.selected_server_removed(), Some(1));
    assert_eq!(state.section, Section::Servers);
    assert_eq!(state.screen, Screen::Browse);
    assert_eq!(state.pending_generation, None);
    assert_eq!(state.load_state, LoadState::Ready);
    assert!(!state.filter_active);
    assert!(state.query.is_empty());
    assert_eq!(state.selected_index, 0);
}

#[test]
fn a_preview_opens_only_on_its_own_result_and_only_it_can_be_inserted() {
    let mut state = McpMenuState::open(1);
    assert_eq!(state.apply(Event::BeginPreview), None);
    assert_eq!(state.apply(Event::InsertPreview), None);
    state.apply(Event::CycleSection(1));
    state.apply(Event::CatalogLoaded {
        generation: 1,
        item_count: 2,
    });
    state.apply(Event::BeginFilter);
    assert_eq!(
        state.apply(Event::BeginPreview),
        Some(Effect::LoadPreview { generation: 2 })
    );
    assert!(!state.filter_active);
    assert_eq!(state.screen, Screen::Browse);
    assert_eq!(state.apply(Event::InsertPreview), None);
    state.apply(Event::PreviewLoaded(9));
    assert_eq!(state.screen, Screen::Browse);
    state.window_start = 4;
    state.apply(Event::PreviewLoaded(2));
    assert_eq!(state.screen, Screen::Preview);
    assert_eq!(state.window_start, 0);
    assert_eq!(state.load_state, LoadState::Ready);
    assert_eq!(
        state.apply(Event::InsertPreview),
        Some(Effect::Insert { generation: 3 })
    );
    assert_eq!(state.load_state, LoadState::Loading);
}

#[test]
fn preview_scrolling_stays_within_its_visual_rows() {
    let mut state = McpMenuState::open(1);
    let scroll = |delta| Event::ScrollPreview {
        delta,
        row_count: 3,
    };
    state.apply(scroll(1));
    assert_eq!(state.window_start, 0);
    state.screen = Screen::Preview;
    state.apply(scroll(1));
    assert_eq!(state.window_start, 1);
    state.apply(scroll(1));
    state.apply(scroll(1));
    assert_eq!(state.window_start, 2);
    state.apply(scroll(-1));
    assert_eq!(state.window_start, 1);
    state.apply(scroll(-1));
    state.apply(scroll(-1));
    assert_eq!(state.window_start, 0);
}
