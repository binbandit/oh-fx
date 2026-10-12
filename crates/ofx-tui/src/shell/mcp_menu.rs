use ofx_contract::{McpMenuAuthentication, McpMenuConnection, McpMenuServer, McpMenuTransport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
    Browse,
    Details,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadState {
    Loading,
    Ready,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Refresh,
    Authenticate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Move {
    pub(crate) delta: isize,
    pub(crate) item_count: usize,
    pub(crate) visible_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    Back,
    Move(Move),
    ShowDetails,
    ShowInfo,
    RequestAction(Action),
    ActionSucceeded(u64),
    EffectFailed(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    Close,
    Action { generation: u64, action: Action },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpMenuState {
    pub(crate) screen: Screen,
    pub(crate) selected_index: usize,
    pub(crate) selected_server_index: usize,
    pub(crate) window_start: usize,
    pub(crate) load_state: LoadState,
    next_generation: u64,
    pub(crate) pending_generation: Option<u64>,
    pending_server: bool,
}

pub(crate) fn server_action_available(action: Action, server: &McpMenuServer) -> bool {
    match action {
        Action::Authenticate => {
            server.transport == McpMenuTransport::Http
                && server.connection != McpMenuConnection::Connecting
                && (server.authentication == McpMenuAuthentication::Required
                    || (server.connection == McpMenuConnection::Disconnected
                        && server.authentication != McpMenuAuthentication::Authenticated))
        }
        Action::Refresh => true,
    }
}

fn targets_selected_server(action: Action) -> bool {
    match action {
        Action::Authenticate => true,
        Action::Refresh => false,
    }
}

impl McpMenuState {
    pub(crate) fn open(server_count: usize) -> Self {
        let mut state = Self {
            screen: Screen::Browse,
            selected_index: 0,
            selected_server_index: 0,
            window_start: 0,
            load_state: LoadState::Ready,
            next_generation: 1,
            pending_generation: None,
            pending_server: false,
        };
        state.clamp_selection(server_count, server_count);
        state
    }

    pub(crate) fn after_generation(mut self, generation: u64) -> Self {
        self.next_generation = generation.wrapping_add(1).max(1);
        self
    }

    pub(crate) fn selected_server_removed(&mut self) {
        if self.pending_server || self.pending_generation.is_none() {
            self.pending_generation = None;
            self.load_state = LoadState::Ready;
        }
        self.screen = Screen::Browse;
    }

    pub(crate) fn apply(&mut self, event: Event) -> Option<Effect> {
        match event {
            Event::Back if self.screen == Screen::Browse => return Some(Effect::Close),
            Event::Back => {
                self.screen = Screen::Browse;
                if self.pending_generation.take().is_some() {
                    self.load_state = LoadState::Cancelled;
                }
            }
            Event::Move(movement) => self.moved(movement),
            Event::ShowDetails => self.screen = Screen::Details,
            Event::ShowInfo => self.screen = Screen::Info,
            Event::RequestAction(action) => return Some(self.begin_action(action)),
            Event::ActionSucceeded(generation) => self.settle(generation, LoadState::Ready),
            Event::EffectFailed(generation) => self.settle(generation, LoadState::Failed),
        }
        None
    }

    fn moved(&mut self, movement: Move) {
        let Move {
            delta,
            item_count,
            visible_count,
        } = movement;
        if item_count == 0 || delta == 0 {
            return;
        }
        self.selected_index = if delta > 0 {
            (self.selected_index + 1) % item_count
        } else if self.selected_index == 0 {
            item_count - 1
        } else {
            self.selected_index - 1
        };
        self.clamp_selection(item_count, visible_count);
        self.selected_server_index = self.selected_index;
    }

    fn begin_action(&mut self, action: Action) -> Effect {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.pending_generation = Some(generation);
        self.pending_server = targets_selected_server(action);
        self.load_state = LoadState::Loading;
        Effect::Action { generation, action }
    }

    fn settle(&mut self, generation: u64, load_state: LoadState) {
        if self.pending_generation != Some(generation) {
            return;
        }
        self.pending_generation = None;
        self.load_state = load_state;
    }

    fn clamp_selection(&mut self, item_count: usize, visible_count: usize) {
        if item_count == 0 {
            self.selected_index = 0;
            self.window_start = 0;
            return;
        }
        self.selected_index = self.selected_index.min(item_count - 1);
        let visible = visible_count.min(item_count).max(1);
        if self.selected_index < self.window_start {
            self.window_start = self.selected_index;
        }
        if self.selected_index >= self.window_start + visible {
            self.window_start = self.selected_index + 1 - visible;
        }
        self.window_start = self.window_start.min(item_count - visible);
    }
}

#[cfg(test)]
mod tests;
