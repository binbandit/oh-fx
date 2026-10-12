use ofx_contract::{
    McpMenuCancel, McpMenuCatalog, McpMenuCatalogRequest, McpMenuPreview, McpMenuRequest, UiCommand,
};

use super::Shell;
use super::mcp_menu::{Effect, Event, Section};
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

    pub(super) fn open_mcp_menu_item(&mut self) {
        let Some(menu) = &self.mcp_menu else {
            return;
        };
        let state = &menu.state;
        let Some(target) = menu.catalogs.preview_target(
            state.section,
            &state.query,
            state.selected_index,
            menu.selected_server_name(),
        ) else {
            return;
        };
        if let Some(Effect::LoadPreview { generation }) =
            self.apply_mcp_menu_event(Event::BeginPreview)
        {
            self.send(UiCommand::McpMenu {
                request: McpMenuRequest::LoadPreview { generation, target },
            });
        }
    }

    pub(super) fn mcp_menu_previewed(&mut self, generation: u64, preview: McpMenuPreview) {
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        if menu.state.pending_generation != Some(generation) {
            return;
        }
        menu.preview = Some(preview);
        menu.feedback = None;
        menu.state.apply(Event::PreviewLoaded(generation));
        self.invalidate();
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
        if self
            .composer
            .insert_text(&insert, COMPOSER_INPUT_LIMIT_BYTES)
            == InsertResult::LimitExceeded
        {
            self.report_limit();
        } else {
            self.mcp_menu = None;
        }
    }

    pub(super) fn cancel_mcp_menu_operation(&mut self, generation: u64, reason: McpMenuCancel) {
        self.send(UiCommand::McpMenu {
            request: McpMenuRequest::Cancel { generation, reason },
        });
    }

    pub(super) fn mcp_menu_loaded(&mut self, generation: u64, catalog: McpMenuCatalog) {
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        if menu.state.pending_generation != Some(generation) {
            return;
        }
        match catalog {
            McpMenuCatalog::Tools(tools) => menu.catalogs.tools = tools,
            McpMenuCatalog::Resources(resources) => menu.catalogs.resources = resources,
            McpMenuCatalog::Prompts(prompts) => menu.catalogs.prompts = prompts,
        }
        let item_count = menu.catalogs.count(menu.state.section, &menu.state.query);
        menu.feedback = None;
        menu.reveal_due_ms = None;
        menu.state.apply(Event::CatalogLoaded {
            generation,
            item_count,
        });
        self.invalidate();
    }
}
