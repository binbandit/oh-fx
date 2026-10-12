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
    assert_eq!(state.apply(Event::Back), None);
    assert_eq!(state.screen, Screen::Browse);
    assert_eq!(state.load_state, LoadState::Cancelled);
    assert_eq!(state.pending_generation, None);
    state.apply(Event::ActionSucceeded(1));
    assert_eq!(state.load_state, LoadState::Cancelled);
    assert_eq!(state.apply(Event::Back), Some(Effect::Close));
}
