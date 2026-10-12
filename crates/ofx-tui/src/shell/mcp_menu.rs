#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
    Browse,
    Details,
    Info,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpMenuState {
    pub(crate) screen: Screen,
    pub(crate) selected_index: usize,
    pub(crate) selected_server_index: usize,
    pub(crate) window_start: usize,
}

impl McpMenuState {
    pub(crate) fn open(server_count: usize) -> Self {
        let mut state = Self {
            screen: Screen::Browse,
            selected_index: 0,
            selected_server_index: 0,
            window_start: 0,
        };
        state.clamp_selection(server_count, server_count);
        state
    }

    pub(crate) fn apply(&mut self, event: Event) -> Option<Effect> {
        match event {
            Event::Back if self.screen == Screen::Browse => return Some(Effect::Close),
            Event::Back => self.screen = Screen::Browse,
            Event::Move(movement) => self.moved(movement),
            Event::ShowDetails => self.screen = Screen::Details,
            Event::ShowInfo => self.screen = Screen::Info,
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
