use ofx_contract::{McpMenuRequest, McpMenuSnapshot, UiEvent};

use super::McpHost;

impl McpHost {
    pub(crate) fn menu_snapshot(&self) -> McpMenuSnapshot {
        McpMenuSnapshot {
            profile_config: self
                .sources
                .profile_path()
                .map(|path| path.display().to_string()),
            ..self.runtime.menu_snapshot()
        }
    }

    pub(crate) fn handle_menu(&self, request: McpMenuRequest) {
        match request {
            McpMenuRequest::Refresh => (self.emit)(UiEvent::McpMenuRefreshed {
                snapshot: self.menu_snapshot(),
            }),
        }
    }
}

#[cfg(test)]
mod tests;
