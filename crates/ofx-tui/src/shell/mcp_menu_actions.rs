use ofx_contract::{McpMenuAddForm, McpMenuAddTransport, McpMenuRequest, McpMenuTrust};

use super::Shell;
use super::mcp_menu::{Action, Event, Screen, Section, server_action_available};
use crate::input::{Action as InputAction, InputEvent, RawTerminalInput, ShortcutAction};

const CTRL_Z: u8 = 26;
const SERVER_NOT_FOUND: &str = "MCP operation failed: McpServerNotFound";

impl Shell<'_> {
    pub(super) fn mcp_menu_request(
        &self,
        generation: u64,
        action: Action,
        server: Option<String>,
    ) -> Option<McpMenuRequest> {
        Some(match action {
            Action::Refresh => McpMenuRequest::Reload { generation },
            Action::Authenticate => McpMenuRequest::Authenticate {
                generation,
                server: server?,
            },
            Action::Logout => McpMenuRequest::Logout {
                generation,
                server: server?,
            },
            Action::Remove => McpMenuRequest::Remove {
                generation,
                server: server?,
            },
            Action::TrustApprove => McpMenuRequest::Trust {
                generation,
                change: McpMenuTrust::Approve(server?),
            },
            Action::TrustReject => McpMenuRequest::Trust {
                generation,
                change: McpMenuTrust::Reject(server?),
            },
            Action::TrustApproveAll => McpMenuRequest::Trust {
                generation,
                change: McpMenuTrust::ApproveAll,
            },
            Action::TrustReset => McpMenuRequest::Trust {
                generation,
                change: McpMenuTrust::Reset,
            },
            Action::AddLocal | Action::AddHttp => McpMenuRequest::Add {
                generation,
                form: self.mcp_add_form()?,
            },
        })
    }

    pub(super) fn fail_mcp_menu_request(&mut self, generation: u64) {
        if let Some(menu) = &mut self.mcp_menu {
            menu.feedback = Some(SERVER_NOT_FOUND.to_owned());
            menu.state.apply(Event::EffectFailed(generation));
        }
    }

    pub(super) fn mcp_menu_primary_action(&mut self) {
        let Some(menu) = &self.mcp_menu else {
            return;
        };
        match menu.state.screen {
            Screen::Details => {
                if menu
                    .view("")
                    .selected_server()
                    .is_some_and(|server| server_action_available(Action::TrustApprove, server))
                {
                    self.request_mcp_menu_action(Action::TrustApprove);
                }
            }
            Screen::Browse if menu.state.section == Section::Servers => {
                self.apply_mcp_menu_event(Event::ShowAdd);
            }
            Screen::Browse
            | Screen::Add
            | Screen::Info
            | Screen::Confirm
            | Screen::Preview
            | Screen::Arguments => {}
        }
    }

    pub(super) fn confirm_mcp_menu_action(&mut self, action: Action) {
        let Some(menu) = &self.mcp_menu else {
            return;
        };
        let screen = menu.state.screen;
        let anywhere = matches!(action, Action::TrustApproveAll | Action::TrustReset);
        let on_servers = screen == Screen::Browse && menu.state.section == Section::Servers;
        if screen != Screen::Details && !(on_servers && anywhere) {
            return;
        }
        if screen == Screen::Details
            && !menu
                .view("")
                .selected_server()
                .is_some_and(|server| server_action_available(action, server))
        {
            return;
        }
        let target = menu.selected_server_name();
        self.apply_mcp_menu_event(Event::ShowConfirmation(action));
        if let Some(menu) = &mut self.mcp_menu {
            menu.confirmation_target = target;
        }
    }

    pub(super) fn route_mcp_add_key(&mut self, event: &InputEvent) -> bool {
        let pending = self
            .mcp_menu
            .as_ref()
            .is_some_and(|menu| menu.state.pending_generation.is_some());
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                b'\t' | b'\r' if pending => {}
                b'\t' => {
                    self.commit_mcp_add_draft();
                    self.apply_mcp_menu_event(Event::CycleAddTransport);
                    self.composer.clear();
                }
                b'\r' => self.advance_mcp_add_form(),
                _ => return self.route_mcp_form_byte(*raw),
            },
            InputEvent::Action(decoded) => match decoded.action {
                InputAction::Escape => {
                    self.composer.clear();
                    self.mcp_menu_back();
                }
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
            InputEvent::Text(_)
            | InputEvent::Paste(_)
            | InputEvent::TextDropped(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => return false,
        }
        true
    }

    pub(super) fn route_mcp_form_byte(&mut self, raw: RawTerminalInput) -> bool {
        match raw.byte {
            b'$' => self.insert("$"),
            CTRL_Z => return false,
            byte if byte == b' ' || byte.is_ascii_graphic() => return false,
            _ => match raw.composer_shortcut {
                Some(ShortcutAction::InsertNewline | ShortcutAction::HistoryNext) | None => {}
                Some(action) => self.route_shortcut(action),
            },
        }
        true
    }

    fn advance_mcp_add_form(&mut self) {
        self.commit_mcp_add_draft();
        self.composer.clear();
        let Some(state) = self.mcp_menu.as_ref().map(|menu| &menu.state) else {
            return;
        };
        let local = state.add_transport == McpMenuAddTransport::Local;
        let field_count = if local { 3 } else { 2 };
        if state.add_field_index + 1 < field_count {
            self.apply_mcp_menu_event(Event::MoveAddField {
                delta: 1,
                field_count,
            });
        } else {
            self.request_mcp_menu_action(if local {
                Action::AddLocal
            } else {
                Action::AddHttp
            });
        }
    }

    fn commit_mcp_add_draft(&mut self) {
        let draft = self.composer.text().to_owned();
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        let field = match menu.state.add_field_index {
            0 => &mut menu.add.name,
            1 => &mut menu.add.target,
            _ => &mut menu.add.arguments,
        };
        *field = draft;
    }

    fn mcp_add_form(&self) -> Option<McpMenuAddForm> {
        let menu = self.mcp_menu.as_ref()?;
        Some(McpMenuAddForm {
            transport: menu.state.add_transport,
            name: menu.add.name.clone(),
            target: menu.add.target.clone(),
            arguments: menu.add.arguments.clone(),
        })
    }
}
