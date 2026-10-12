use ofx_contract::{
    McpMenuCancel, McpMenuOutcome, McpMenuRequest, McpMenuServer, McpMenuSettled, McpMenuSnapshot,
    UiCommand, UiEvent,
};

use super::Shell;
use super::mcp_menu::{
    Action, Effect, Event, McpMenuState, Move, Screen, Section, server_action_available,
};
use crate::footer::mcp_menu_presentation::{
    AddFields, MAX_INLINE_ROWS, McpMenuCatalogs, McpMenuView, mcp_menu_hint_row, mcp_menu_rows,
    menu_row_count, visible_items_for_budget,
};
use crate::input::{Action as InputAction, InputEvent};
use crate::row_text::Row;

const REFRESH_INTERVAL_MS: i64 = 100;
const CTRL_Z: u8 = 26;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct McpMenu {
    pub(super) state: McpMenuState,
    snapshot: McpMenuSnapshot,
    pub(super) feedback: Option<String>,
    pub(super) add: AddForm,
    pub(super) confirmation_target: Option<String>,
    pub(super) catalogs: McpMenuCatalogs,
    refresh_due_ms: Option<i64>,
    pub(super) reveal_due_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct AddForm {
    pub(super) name: String,
    pub(super) target: String,
    pub(super) arguments: String,
}

impl McpMenu {
    pub(super) fn view<'a>(&'a self, draft: &'a str) -> McpMenuView<'a> {
        McpMenuView {
            state: &self.state,
            snapshot: &self.snapshot,
            feedback: self.feedback.as_deref(),
            add: AddFields {
                name: &self.add.name,
                target: &self.add.target,
                arguments: &self.add.arguments,
                draft,
            },
            catalogs: &self.catalogs,
            loading_visible: self.reveal_due_ms.is_none(),
        }
    }

    fn target_server_name(&self) -> Option<String> {
        if self.state.screen == Screen::Confirm {
            return self.confirmation_target.clone();
        }
        self.selected_server_name()
    }

    pub(super) fn selected_server_name(&self) -> Option<String> {
        self.snapshot
            .servers
            .get(self.state.selected_server_index)
            .map(|server| server.name.clone())
    }

    fn replace_snapshot(&mut self, next: McpMenuSnapshot) -> (bool, Option<u64>) {
        if self.snapshot == next {
            return (false, None);
        }
        let mut dropped = None;
        if let Some(previous) = self.snapshot.servers.get(self.state.selected_server_index) {
            let kept = next
                .servers
                .iter()
                .position(|server| same_server(server, previous));
            self.state.selected_server_index = kept.unwrap_or(0);
            if self.state.section == Section::Servers {
                self.state.selected_index = self.state.selected_server_index;
            }
            if kept.is_none() {
                dropped = self.state.selected_server_removed();
            }
        }
        self.snapshot = next;
        (true, dropped)
    }
}

fn same_server(server: &McpMenuServer, previous: &McpMenuServer) -> bool {
    server.source == previous.source && server.name == previous.name
}

impl Shell<'_> {
    fn open_mcp_menu(&mut self, snapshot: McpMenuSnapshot) {
        self.skills_menu = None;
        self.help_menu = None;
        self.settings_menu = None;
        self.statusline_menu = None;
        self.workspace_menu = None;
        self.usage_menu = None;
        self.model_menu = None;
        self.close_picker();
        self.mcp_menu = Some(McpMenu {
            state: McpMenuState::open(snapshot.servers.len())
                .after_generation(self.mcp_menu_generation),
            snapshot,
            feedback: None,
            add: AddForm::default(),
            confirmation_target: None,
            catalogs: McpMenuCatalogs::default(),
            refresh_due_ms: Some(self.now_ms() + REFRESH_INTERVAL_MS),
            reveal_due_ms: None,
        });
        self.invalidate();
    }

    pub(super) fn mcp_menu_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::McpMenuOpened { snapshot } => self.open_mcp_menu(snapshot),
            UiEvent::McpMenuRefreshed { snapshot } => self.mcp_menu_refreshed(snapshot),
            UiEvent::McpMenuSettled { settled, snapshot } => {
                self.mcp_menu_settled(settled, snapshot);
            }
            UiEvent::McpMenuLoaded {
                generation,
                catalog,
            } => self.mcp_menu_loaded(generation, catalog),
            _ => {}
        }
    }

    fn mcp_menu_refreshed(&mut self, snapshot: McpMenuSnapshot) {
        let due = self.now_ms() + REFRESH_INTERVAL_MS;
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        menu.refresh_due_ms = Some(due);
        if self.replace_mcp_menu_snapshot(snapshot) {
            self.invalidate();
        }
    }

    fn replace_mcp_menu_snapshot(&mut self, snapshot: McpMenuSnapshot) -> bool {
        let Some(menu) = &mut self.mcp_menu else {
            return false;
        };
        let (changed, dropped) = menu.replace_snapshot(snapshot);
        if let Some(generation) = dropped {
            self.cancel_mcp_menu_operation(generation, McpMenuCancel::ServerRemoved);
        }
        changed
    }

    fn mcp_menu_settled(&mut self, settled: McpMenuSettled, snapshot: McpMenuSnapshot) {
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        if menu.state.pending_generation != Some(settled.generation) {
            return;
        }
        menu.reveal_due_ms = None;
        menu.feedback = Some(settled.feedback);
        if matches!(
            settled.outcome,
            McpMenuOutcome::Continuing | McpMenuOutcome::Returned
        ) {
            menu.state.return_to_servers();
        }
        match settled.outcome {
            McpMenuOutcome::Continuing => {}
            McpMenuOutcome::Returned | McpMenuOutcome::Succeeded => {
                menu.state.apply(Event::ActionSucceeded(settled.generation));
            }
            McpMenuOutcome::Failed => {
                menu.state.apply(Event::EffectFailed(settled.generation));
            }
        }
        self.replace_mcp_menu_snapshot(snapshot);
        self.invalidate();
    }

    pub(super) fn mcp_menu_refresh_due_ms(&self) -> Option<i64> {
        let menu = self.mcp_menu.as_ref()?;
        menu.refresh_due_ms
            .into_iter()
            .chain(menu.reveal_due_ms)
            .min()
    }

    pub(super) fn refresh_mcp_menu(&mut self, now_ms: i64) {
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        if menu.reveal_due_ms.is_some_and(|due| due <= now_ms) {
            menu.reveal_due_ms = None;
            self.invalidate();
        }
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
        let view = menu.view(self.composer.text());
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
        if self
            .mcp_menu
            .as_ref()
            .is_some_and(|menu| menu.state.screen == Screen::Add)
        {
            return self.route_mcp_add_key(event);
        }
        if self
            .mcp_menu
            .as_ref()
            .is_some_and(|menu| menu.state.filter_active)
            && self.route_mcp_filter_key(event)
        {
            return true;
        }
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                CTRL_Z => return false,
                b'\t' => {
                    self.apply_mcp_menu_event(Event::CycleSection(1));
                }
                b'/' => {
                    self.apply_mcp_menu_event(Event::BeginFilter);
                }
                b'\r' => self.submit_mcp_menu_selection(),
                b'a' | b'A' => self.mcp_menu_primary_action(),
                b'c' | b'C' => {
                    self.apply_mcp_menu_event(Event::ShowInfo);
                }
                b'd' | b'D' => self.confirm_mcp_menu_action(Action::Remove),
                b'l' | b'L' => self.confirm_mcp_menu_action(Action::Logout),
                b'x' | b'X' => self.confirm_mcp_menu_action(Action::TrustReject),
                b'p' | b'P' => self.confirm_mcp_menu_action(Action::TrustApproveAll),
                b'z' | b'Z' => self.confirm_mcp_menu_action(Action::TrustReset),
                b'r' | b'R' => self.reload_from_mcp_menu(),
                10 => self.move_mcp_menu(1),
                11 => self.move_mcp_menu(-1),
                _ => {}
            },
            InputEvent::Action(decoded) => match decoded.action {
                InputAction::Escape => self.mcp_menu_back(),
                InputAction::CursorUp => self.move_mcp_menu(-1),
                InputAction::CursorDown => self.move_mcp_menu(1),
                InputAction::CursorLeft => {
                    self.apply_mcp_menu_event(Event::CycleSection(-1));
                }
                InputAction::CursorRight => {
                    self.apply_mcp_menu_event(Event::CycleSection(1));
                }
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

    pub(super) fn mcp_menu_back(&mut self) {
        self.gestures.disarm_escape_clear();
        self.gestures.disarm_escape_interrupt();
        let filtering = self
            .mcp_menu
            .as_ref()
            .is_some_and(|menu| menu.state.filter_active);
        self.apply_mcp_menu_event(if filtering {
            Event::ClearFilter
        } else {
            Event::Back
        });
    }

    pub(super) fn apply_mcp_menu_event(&mut self, event: Event) -> Option<Effect> {
        let menu = self.mcp_menu.as_mut()?;
        let pending = menu.state.pending_generation;
        let effect = menu.state.apply(event);
        match effect {
            Some(Effect::Close) => {
                self.mcp_menu = None;
                self.composer.clear();
                if let Some(generation) = pending {
                    self.cancel_mcp_menu_operation(generation, McpMenuCancel::Closed);
                }
            }
            Some(Effect::Action { generation, .. }) => self.mcp_menu_generation = generation,
            Some(Effect::LoadCatalog {
                generation,
                section,
            }) => {
                self.mcp_menu_generation = generation;
                self.load_mcp_menu_catalog(generation, section);
            }
            Some(Effect::Cancel(generation)) => {
                self.cancel_mcp_menu_operation(generation, McpMenuCancel::Back);
            }
            None => {}
        }
        effect
    }

    pub(super) fn move_mcp_menu(&mut self, delta: isize) {
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        let item_count = menu.view("").item_count();
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
        let view = menu.view("");
        match menu.state.screen {
            Screen::Browse if view.item_count() > 0 => {
                self.apply_mcp_menu_event(Event::ShowDetails);
            }
            Screen::Details => {
                if view
                    .selected_server()
                    .is_some_and(|server| server_action_available(Action::Authenticate, server))
                {
                    self.request_mcp_menu_action(Action::Authenticate);
                }
            }
            Screen::Confirm if menu.state.pending_generation.is_none() => {
                if let Some(action) = menu.state.confirmation_action {
                    self.request_mcp_menu_action(action);
                }
            }
            Screen::Browse | Screen::Add | Screen::Info | Screen::Confirm => {}
        }
    }

    fn reload_from_mcp_menu(&mut self) {
        if self.mcp_menu.as_ref().is_some_and(|menu| {
            menu.state.screen == Screen::Browse && menu.state.section == Section::Servers
        }) {
            self.request_mcp_menu_action(Action::Refresh);
        }
    }

    pub(super) fn request_mcp_menu_action(&mut self, action: Action) {
        let Some(server) = self.mcp_menu.as_ref().map(McpMenu::target_server_name) else {
            return;
        };
        let Some(Effect::Action { generation, action }) =
            self.apply_mcp_menu_event(Event::RequestAction(action))
        else {
            return;
        };
        if action == Action::Logout
            && let Some(menu) = &mut self.mcp_menu
        {
            menu.feedback = None;
        }
        match self.mcp_menu_request(generation, action, server) {
            Some(request) => self.send(UiCommand::McpMenu { request }),
            None => self.fail_mcp_menu_request(generation),
        }
    }
}

#[cfg(test)]
mod tests;
