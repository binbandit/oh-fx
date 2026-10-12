use std::sync::Arc;

use ofx_contract::McpMenuSnapshot;

use super::{McpRuntime, lock};
use crate::project_config::render_workspace_diagnostic;
use crate::server_lifecycle::Server;
use crate::server_menu::menu_server;

impl McpRuntime {
    pub fn menu_snapshot(&self) -> McpMenuSnapshot {
        McpMenuSnapshot {
            servers: self
                .current()
                .iter()
                .map(|server| menu_server(server))
                .collect(),
            configuration_issues: lock(&self.workspace_diagnostics)
                .iter()
                .map(render_workspace_diagnostic)
                .collect(),
            profile_config: None,
        }
    }

    pub fn menu_tool_names(&self) -> Vec<String> {
        let mut names = lock(&self.names);
        let mut listed = Vec::new();
        for server in self.current() {
            let Some((catalog, _)) = server.catalog() else {
                continue;
            };
            for tool in &catalog.tools {
                if let Ok(name) = names.name(&self.reserved, &server.config.name, &tool.name) {
                    listed.push(name);
                }
            }
        }
        listed
    }
}

pub(super) struct ReloadMarks(Vec<Arc<Server>>);

impl ReloadMarks {
    pub(super) fn set(servers: Vec<Arc<Server>>) -> Self {
        for server in &servers {
            server.mark_reload_pending(true);
        }
        Self(servers)
    }
}

impl Drop for ReloadMarks {
    fn drop(&mut self) {
        for server in &self.0 {
            server.mark_reload_pending(false);
        }
    }
}

#[cfg(test)]
mod tests;
