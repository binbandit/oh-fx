use ofx_contract::{
    McpMenuAddTransport, McpMenuAdmission, McpMenuAuthentication, McpMenuConnection, McpMenuServer,
    McpMenuSource, McpMenuTransport,
};

const MAX_QUERY_BYTES: usize = 128;
const SECTIONS: [Section; 4] = [
    Section::Servers,
    Section::Tools,
    Section::Resources,
    Section::Prompts,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Servers,
    Tools,
    Resources,
    Prompts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
    Browse,
    Details,
    Add,
    Info,
    Confirm,
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
    Logout,
    AddLocal,
    AddHttp,
    Remove,
    TrustApprove,
    TrustReject,
    TrustApproveAll,
    TrustReset,
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
    CycleAddTransport,
    MoveAddField { delta: isize, field_count: usize },
    ShowDetails,
    ShowAdd,
    ShowInfo,
    ShowConfirmation(Action),
    RequestAction(Action),
    CycleSection(isize),
    BeginFilter,
    AppendFilterByte(u8),
    DeleteFilterByte,
    ClearFilterText,
    ClearFilter,
    CatalogLoaded { generation: u64, item_count: usize },
    ActionSucceeded(u64),
    EffectFailed(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    Close,
    Action { generation: u64, action: Action },
    LoadCatalog { generation: u64, section: Section },
    Cancel(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpMenuState {
    pub(crate) section: Section,
    pub(crate) screen: Screen,
    pub(crate) selected_index: usize,
    pub(crate) selected_server_index: usize,
    pub(crate) window_start: usize,
    pub(crate) load_state: LoadState,
    next_generation: u64,
    pub(crate) pending_generation: Option<u64>,
    pending_server: bool,
    pub(crate) confirmation_action: Option<Action>,
    pub(crate) add_transport: McpMenuAddTransport,
    pub(crate) add_field_index: usize,
    pub(crate) filter_active: bool,
    pub(crate) query: String,
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
        Action::Logout => {
            server.transport == McpMenuTransport::Http
                && server.authentication == McpMenuAuthentication::Authenticated
        }
        Action::Remove => server.source == McpMenuSource::Profile,
        Action::TrustApprove => server.admission == Some(McpMenuAdmission::Pending),
        Action::TrustReject => {
            server.source == McpMenuSource::Project
                && server.admission != Some(McpMenuAdmission::Rejected)
        }
        Action::Refresh
        | Action::AddLocal
        | Action::AddHttp
        | Action::TrustApproveAll
        | Action::TrustReset => true,
    }
}

fn requires_confirmation(action: Action) -> bool {
    match action {
        Action::Logout
        | Action::Remove
        | Action::TrustReject
        | Action::TrustApproveAll
        | Action::TrustReset => true,
        Action::Refresh
        | Action::Authenticate
        | Action::AddLocal
        | Action::AddHttp
        | Action::TrustApprove => false,
    }
}

fn targets_selected_server(action: Action) -> bool {
    match action {
        Action::Authenticate
        | Action::Logout
        | Action::Remove
        | Action::TrustApprove
        | Action::TrustReject => true,
        Action::Refresh
        | Action::AddLocal
        | Action::AddHttp
        | Action::TrustApproveAll
        | Action::TrustReset => false,
    }
}

impl McpMenuState {
    pub(crate) fn open(server_count: usize) -> Self {
        let mut state = Self {
            section: Section::Servers,
            screen: Screen::Browse,
            selected_index: 0,
            selected_server_index: 0,
            window_start: 0,
            load_state: LoadState::Ready,
            next_generation: 1,
            pending_generation: None,
            pending_server: false,
            confirmation_action: None,
            add_transport: McpMenuAddTransport::Local,
            add_field_index: 0,
            filter_active: false,
            query: String::new(),
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
        if self.screen != Screen::Add {
            self.section = Section::Servers;
            self.screen = Screen::Browse;
            self.clear_filter();
            self.selected_index = self.selected_server_index;
        }
    }

    pub(crate) fn apply(&mut self, event: Event) -> Option<Effect> {
        match event {
            Event::Back if self.screen == Screen::Browse => return Some(Effect::Close),
            Event::Back => {
                self.screen = Screen::Browse;
                self.confirmation_action = None;
                let pending = self.pending_generation.take();
                if pending.is_some() {
                    self.load_state = LoadState::Cancelled;
                }
                return pending.map(Effect::Cancel);
            }
            Event::Move(movement) if self.screen != Screen::Confirm => self.moved(movement),
            Event::CycleAddTransport if self.screen == Screen::Add => {
                self.add_transport = match self.add_transport {
                    McpMenuAddTransport::Local => McpMenuAddTransport::Http,
                    McpMenuAddTransport::Http => McpMenuAddTransport::Local,
                };
                self.add_field_index = 0;
            }
            Event::MoveAddField { delta, field_count }
                if self.screen == Screen::Add && field_count > 0 && delta != 0 =>
            {
                self.add_field_index = if delta > 0 {
                    (self.add_field_index + 1) % field_count
                } else if self.add_field_index == 0 {
                    field_count - 1
                } else {
                    self.add_field_index - 1
                };
            }
            Event::ShowDetails if self.section == Section::Servers => {
                self.screen = Screen::Details;
            }
            Event::ShowInfo if self.section == Section::Servers => self.screen = Screen::Info,
            Event::Move(_)
            | Event::CycleAddTransport
            | Event::MoveAddField { .. }
            | Event::ShowDetails
            | Event::ShowInfo => {}
            Event::ShowAdd => {
                self.screen = Screen::Add;
                self.add_field_index = 0;
            }
            Event::ShowConfirmation(action) => {
                self.screen = Screen::Confirm;
                self.confirmation_action = Some(action);
            }
            Event::RequestAction(action)
                if requires_confirmation(action)
                    && (self.screen != Screen::Confirm
                        || self.confirmation_action != Some(action)) => {}
            Event::RequestAction(action) => return Some(self.begin_action(action)),
            Event::CycleSection(delta) => return self.cycle_section(delta),
            Event::BeginFilter => {
                if self.screen == Screen::Browse && self.section != Section::Servers {
                    self.filter_active = true;
                    self.reset_navigation();
                }
            }
            Event::AppendFilterByte(byte) => {
                if self.filter_active
                    && self.query.len() < MAX_QUERY_BYTES
                    && (0x20..0x7f).contains(&byte)
                {
                    self.query.push(char::from(byte));
                    self.reset_navigation();
                }
            }
            Event::DeleteFilterByte => {
                if self.filter_active && self.query.pop().is_some() {
                    self.reset_navigation();
                }
            }
            Event::ClearFilterText => {
                if self.filter_active {
                    self.query.clear();
                    self.reset_navigation();
                }
            }
            Event::ClearFilter => self.clear_filter(),
            Event::CatalogLoaded {
                generation,
                item_count,
            } => {
                if self.settle(generation, LoadState::Ready) {
                    self.confirmation_action = None;
                    self.clamp_selection(item_count, item_count);
                    self.screen = Screen::Browse;
                }
            }
            Event::ActionSucceeded(generation) => {
                if self.settle(generation, LoadState::Ready) {
                    self.confirmation_action = None;
                }
            }
            Event::EffectFailed(generation) => {
                self.settle(generation, LoadState::Failed);
            }
        }
        None
    }

    pub(crate) fn return_to_servers(&mut self) {
        self.section = Section::Servers;
        self.screen = Screen::Browse;
        self.selected_index = self.selected_server_index;
        self.window_start = 0;
        self.confirmation_action = None;
        self.pending_server = false;
        self.filter_active = false;
        self.query.clear();
    }

    fn cycle_section(&mut self, delta: isize) -> Option<Effect> {
        if delta == 0 {
            return None;
        }
        let index = SECTIONS
            .iter()
            .position(|section| *section == self.section)
            .unwrap_or_default();
        let next = if delta > 0 {
            (index + 1) % SECTIONS.len()
        } else {
            (index + SECTIONS.len() - 1) % SECTIONS.len()
        };
        self.section = SECTIONS[next];
        self.screen = Screen::Browse;
        self.clear_filter();
        if self.section == Section::Servers {
            self.selected_index = self.selected_server_index;
            self.load_state = LoadState::Ready;
            return self.pending_generation.take().map(Effect::Cancel);
        }
        let generation = self.begin_generation(true);
        Some(Effect::LoadCatalog {
            generation,
            section: self.section,
        })
    }

    fn clear_filter(&mut self) {
        self.filter_active = false;
        self.query.clear();
        self.reset_navigation();
    }

    fn reset_navigation(&mut self) {
        self.selected_index = 0;
        self.window_start = 0;
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
        if self.section == Section::Servers {
            self.selected_server_index = self.selected_index;
        }
    }

    fn begin_action(&mut self, action: Action) -> Effect {
        let generation = self.begin_generation(targets_selected_server(action));
        Effect::Action { generation, action }
    }

    fn begin_generation(&mut self, server_bound: bool) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.pending_generation = Some(generation);
        self.pending_server = server_bound;
        self.load_state = LoadState::Loading;
        generation
    }

    fn settle(&mut self, generation: u64, load_state: LoadState) -> bool {
        if self.pending_generation != Some(generation) {
            return false;
        }
        self.pending_generation = None;
        self.load_state = load_state;
        true
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
