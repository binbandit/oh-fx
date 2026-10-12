use ofx_contract::{
    McpMenuCancel, McpMenuCatalog, McpMenuCatalogRequest, McpMenuPreview, McpMenuRequest, UiCommand,
};

use ofx_trace::trace_log;

use super::Shell;
use super::mcp_menu::{Event, Section};
use super::mcp_menu_runtime::McpMenu;
use crate::composer::InsertResult;
use crate::input::{COMPOSER_INPUT_LIMIT_BYTES, InputEvent, ShortcutAction};

const LOADING_GRACE_MS: i64 = 200;
const DELETE: u8 = 0x7f;

impl Shell<'_> {
    pub(super) fn route_mcp_filter_key(&mut self, event: &InputEvent) -> bool {
        let event = match event {
            InputEvent::Raw(raw) if raw.byte == DELETE => Event::DeleteFilterByte,
            InputEvent::Raw(raw) if (0x20..DELETE).contains(&raw.byte) => {
                Event::AppendFilterByte(raw.byte)
            }
            InputEvent::Action(decoded) => match decoded.composer_shortcut {
                Some(ShortcutAction::DeleteBackward) => Event::DeleteFilterByte,
                Some(
                    ShortcutAction::DeleteWordLeft
                    | ShortcutAction::DeleteWhitespaceWordLeft
                    | ShortcutAction::DeleteToLineStart,
                ) => Event::ClearFilterText,
                _ => return false,
            },
            _ => return false,
        };
        self.apply_mcp_menu_event(event);
        true
    }

    pub(super) fn load_mcp_menu_catalog(&mut self, generation: u64, section: Section) {
        let due = self.now_ms() + LOADING_GRACE_MS;
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        menu.feedback = None;
        let server = menu.selected_server_name();
        let catalog = match section {
            Section::Servers | Section::Tools => Some(McpMenuCatalogRequest::Tools),
            Section::Resources => server.map(McpMenuCatalogRequest::Resources),
            Section::Prompts => server.map(McpMenuCatalogRequest::Prompts),
        };
        let Some(catalog) = catalog else {
            let empty = match section {
                Section::Resources => McpMenuCatalog::Resources(Vec::new()),
                _ => McpMenuCatalog::Prompts(Vec::new()),
            };
            self.mcp_menu_loaded(generation, empty);
            return;
        };
        menu.reveal_due_ms = Some(due);
        self.send(UiCommand::McpMenu {
            request: McpMenuRequest::LoadCatalog {
                generation,
                catalog,
            },
        });
    }

    pub(super) fn mcp_menu_previewed(&mut self, generation: u64, preview: McpMenuPreview) {
        let Some(menu) = self.mcp_menu_awaiting(generation) else {
            return;
        };
        menu.preview = Some(preview);
        menu.feedback = None;
        menu.state.apply(Event::PreviewLoaded(generation));
        self.invalidate();
    }

    pub(super) fn mcp_menu_awaiting(&mut self, generation: u64) -> Option<&mut McpMenu> {
        let pending = self
            .mcp_menu
            .as_ref()
            .and_then(|menu| menu.state.pending_generation);
        if pending != Some(generation) {
            trace_log!(
                "mcp",
                "discarding menu completion generation={generation} active={} pending_generation={}",
                self.mcp_menu.is_some(),
                pending.map_or_else(|| "null".to_owned(), |pending| pending.to_string())
            );
            return None;
        }
        self.mcp_menu.as_mut()
    }

    pub(super) fn insert_mcp_menu_preview(&mut self) {
        let Some(insert) = self
            .mcp_menu
            .as_ref()
            .and_then(|menu| menu.preview.as_ref())
            .map(|preview| preview.insert.clone())
        else {
            return;
        };
        if self.apply_mcp_menu_event(Event::InsertPreview).is_none() {
            return;
        }
        match self
            .composer
            .insert_text(&insert, COMPOSER_INPUT_LIMIT_BYTES)
        {
            InsertResult::Inserted => self.close_mcp_menu(),
            InsertResult::LimitExceeded => self.report_limit(),
            InsertResult::Inactive => {}
        }
    }

    pub(super) fn cancel_mcp_menu_operation(&mut self, generation: u64, reason: McpMenuCancel) {
        self.send(UiCommand::McpMenu {
            request: McpMenuRequest::Cancel { generation, reason },
        });
    }

    pub(super) fn mcp_menu_loaded(&mut self, generation: u64, catalog: McpMenuCatalog) {
        let Some(menu) = self.mcp_menu_awaiting(generation) else {
            return;
        };
        let server = menu.selected_server_name();
        match catalog {
            McpMenuCatalog::Tools(tools) => menu.catalogs.tools = tools,
            McpMenuCatalog::Resources(resources) => {
                menu.catalogs.resources = resources;
                menu.catalogs.resources_server.clone_from(&server);
            }
            McpMenuCatalog::Prompts(prompts) => {
                menu.catalogs.prompts = prompts;
                menu.catalogs.prompts_server.clone_from(&server);
            }
        }
        let item_count =
            menu.catalogs
                .count(menu.state.section, &menu.state.query, server.as_deref());
        menu.feedback = None;
        menu.reveal_due_ms = None;
        menu.state.apply(Event::CatalogLoaded {
            generation,
            item_count,
        });
        self.invalidate();
    }
}
