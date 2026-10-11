use super::*;

fn moved(delta: isize, item_count: usize, visible_count: usize) -> Event {
    Event::Move(Move {
        delta,
        item_count,
        visible_count,
    })
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
fn the_menu_opens_on_the_server_list() {
    let state = McpMenuState::open(3);
    assert_eq!(state.screen, Screen::Browse);
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
fn back_leaves_a_screen_and_closes_from_the_server_list() {
    let mut state = McpMenuState::open(2);
    state.apply(Event::ShowInfo);
    assert_eq!(state.screen, Screen::Info);
    assert_eq!(state.apply(Event::Back), None);
    assert_eq!(state.screen, Screen::Browse);
    assert_eq!(state.apply(Event::Back), Some(Effect::Close));
}
