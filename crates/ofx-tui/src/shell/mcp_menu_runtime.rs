use ofx_contract::{McpMenuRequest, McpMenuServer, McpMenuSnapshot, UiCommand};

use super::Shell;
use super::mcp_menu::{Effect, Event, McpMenuState, Move, Screen};
use crate::footer::mcp_menu_presentation::{
    MAX_INLINE_ROWS, McpMenuView, mcp_menu_hint_row, mcp_menu_rows, menu_row_count,
    visible_items_for_budget,
};
use crate::input::{Action as InputAction, InputEvent};
use crate::row_text::Row;

const REFRESH_INTERVAL_MS: i64 = 100;
const CTRL_Z: u8 = 26;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct McpMenu {
    state: McpMenuState,
    snapshot: McpMenuSnapshot,
    refresh_due_ms: Option<i64>,
}

impl McpMenu {
    fn view(&self) -> McpMenuView<'_> {
        McpMenuView {
            state: &self.state,
            snapshot: &self.snapshot,
        }
    }

    fn replace_snapshot(&mut self, next: McpMenuSnapshot) -> bool {
        if self.snapshot == next {
            return false;
        }
        if let Some(previous) = self.snapshot.servers.get(self.state.selected_server_index) {
            let kept = next
                .servers
                .iter()
                .position(|server| same_server(server, previous));
            self.state.selected_server_index = kept.unwrap_or(0);
            self.state.selected_index = self.state.selected_server_index;
            if kept.is_none() {
                self.state.screen = Screen::Browse;
            }
        }
        self.snapshot = next;
        true
    }
}

fn same_server(server: &McpMenuServer, previous: &McpMenuServer) -> bool {
    server.source == previous.source && server.name == previous.name
}

impl Shell<'_> {
    pub(super) fn open_mcp_menu(&mut self, snapshot: McpMenuSnapshot) {
        self.skills_menu = None;
        self.help_menu = None;
        self.settings_menu = None;
        self.statusline_menu = None;
        self.workspace_menu = None;
        self.usage_menu = None;
        self.model_menu = None;
        self.close_picker();
        self.mcp_menu = Some(McpMenu {
            state: McpMenuState::open(snapshot.servers.len()),
            snapshot,
            refresh_due_ms: Some(self.now_ms() + REFRESH_INTERVAL_MS),
        });
        self.invalidate();
    }

    pub(super) fn mcp_menu_refreshed(&mut self, snapshot: McpMenuSnapshot) {
        let due = self.now_ms() + REFRESH_INTERVAL_MS;
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        menu.refresh_due_ms = Some(due);
        if menu.replace_snapshot(snapshot) {
            self.invalidate();
        }
    }

    pub(super) fn mcp_menu_refresh_due_ms(&self) -> Option<i64> {
        self.mcp_menu.as_ref().and_then(|menu| menu.refresh_due_ms)
    }

    pub(super) fn refresh_mcp_menu(&mut self, now_ms: i64) {
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        if menu.refresh_due_ms.is_none_or(|due| due > now_ms) {
            return;
        }
        menu.refresh_due_ms = None;
        self.send(UiCommand::McpMenu {
            request: McpMenuRequest::Refresh,
        });
    }

    pub(super) fn mcp_menu_band(&self) -> Option<(Vec<Row>, Row)> {
        let menu = self.mcp_menu.as_ref()?;
        if self.approval.is_some() || self.question.is_some() {
            return None;
        }
        let view = menu.view();
        let row_count = menu_row_count(view, self.menu_budget(MAX_INLINE_ROWS));
        let rows = mcp_menu_rows(&self.theme, view, self.cols(), row_count);
        let mut band = Vec::with_capacity(rows.len() + 2);
        band.push(Row::new());
        band.extend(rows);
        band.push(Row::new());
        let hint = mcp_menu_hint_row(
            &self.theme,
            view,
            self.cols(),
            self.gestures.ctrl_c_exit_armed(),
        );
        Some((band, hint))
    }

    pub(super) fn mcp_menu_owns(&mut self, event: &InputEvent) -> bool {
        if self.mcp_menu.is_none() {
            return false;
        }
        let owned = self.route_mcp_menu_key(event);
        if owned {
            self.gestures.disarm_ctrl_c_exit();
            self.invalidate();
        }
        owned
    }

    fn route_mcp_menu_key(&mut self, event: &InputEvent) -> bool {
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                CTRL_Z => return false,
                b'\r' => self.submit_mcp_menu_selection(),
                b'c' | b'C' => self.apply_mcp_menu_event(Event::ShowInfo),
                10 => self.move_mcp_menu(1),
                11 => self.move_mcp_menu(-1),
                _ => {}
            },
            InputEvent::Action(decoded) => match decoded.action {
                InputAction::Escape => {
                    self.gestures.disarm_escape_clear();
                    self.gestures.disarm_escape_interrupt();
                    self.apply_mcp_menu_event(Event::Back);
                }
                InputAction::CursorUp => self.move_mcp_menu(-1),
                InputAction::CursorDown => self.move_mcp_menu(1),
                InputAction::CursorLeft | InputAction::CursorRight => {}
                _ => return false,
            },
            InputEvent::Text(_) => {}
            InputEvent::Paste(_)
            | InputEvent::TextDropped(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => return false,
        }
        true
    }

    fn apply_mcp_menu_event(&mut self, event: Event) {
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        if menu.state.apply(event) == Some(Effect::Close) {
            self.mcp_menu = None;
            self.composer.clear();
        }
    }

    fn move_mcp_menu(&mut self, delta: isize) {
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        let item_count = menu.view().item_count();
        menu.state.apply(Event::Move(Move {
            delta,
            item_count,
            visible_count: visible_items_for_budget(MAX_INLINE_ROWS),
        }));
    }

    fn submit_mcp_menu_selection(&mut self) {
        let Some(menu) = &self.mcp_menu else {
            return;
        };
        if menu.state.screen == Screen::Browse && menu.view().item_count() > 0 {
            self.apply_mcp_menu_event(Event::ShowDetails);
        }
    }
}

#[cfg(test)]
mod tests;
