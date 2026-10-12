use std::sync::Arc;

use ofx_contract::{McpMenuOutcome, McpMenuRequest, McpMenuSettled, McpMenuSnapshot, UiEvent};
use ofx_mcp::{
    AuthenticationOutcome, McpError, McpRuntime, ReloadOutcome, authentication_error_message,
};

use super::{AuthenticationRefusal, McpHost, ReloadFailure, Settle};

const RELOADED: &str = "MCP configuration reloaded.";
const RETAINED: &str = "MCP reload failed; the previous runtime remains active.";
const AUTHORITY_REDUCED: &str = "McpAuthorityReducedReloadFailed";
const AUTHENTICATION_BUSY: &str = "McpAuthenticationBusy";

impl McpHost {
    pub(crate) fn menu_snapshot(&self) -> McpMenuSnapshot {
        snapshot(&self.runtime, self.profile_config())
    }

    pub(crate) fn handle_menu(&self, request: McpMenuRequest) {
        match request {
            McpMenuRequest::Refresh => (self.emit)(UiEvent::McpMenuRefreshed {
                snapshot: self.menu_snapshot(),
            }),
            McpMenuRequest::Reload { generation } => self.begin_menu_reload(generation),
            McpMenuRequest::Authenticate { generation, server } => {
                self.begin_menu_authentication(generation, &server);
            }
            McpMenuRequest::Logout { generation, server } => {
                self.begin_menu_logout(generation, server);
            }
            McpMenuRequest::Add { generation, form } => self.add_from_menu(generation, &form),
            McpMenuRequest::Remove { generation, server } => {
                self.remove_from_menu(generation, &server);
            }
            McpMenuRequest::Trust { generation, change } => {
                self.trust_from_menu(generation, change);
            }
            McpMenuRequest::LoadCatalog {
                generation,
                catalog,
            } => self.load_menu_catalog(generation, catalog),
            McpMenuRequest::LoadPreview { generation, target } => {
                self.load_menu_preview(generation, target);
            }
            McpMenuRequest::Cancel { generation, reason } => {
                self.cancel_menu_operation(generation, reason);
            }
        }
    }

    fn settle_menu(&self, generation: u64, outcome: McpMenuOutcome, feedback: String) {
        (self.emit)(settled(
            &self.runtime,
            self.profile_config(),
            generation,
            outcome,
            feedback,
        ));
    }

    fn profile_config(&self) -> Option<String> {
        self.sources
            .profile_path()
            .map(|path| path.display().to_string())
    }

    pub(super) fn begin_menu_reload(&self, generation: u64) {
        self.reload_with(self.menu_settle(generation));
    }

    pub(super) fn menu_settle(&self, generation: u64) -> Settle {
        let runtime = Arc::clone(&self.runtime);
        let profile = self.profile_config();
        Box::new(move |result| {
            let (outcome, feedback) = reload_feedback(result)?;
            Some(settled(&runtime, profile, generation, outcome, feedback))
        })
    }

    fn begin_menu_authentication(&self, generation: u64, server: &str) {
        let runtime = Arc::clone(&self.runtime);
        let profile = self.profile_config();
        let started = self.start_authentication(server, move |name, result, reconnect| {
            let (outcome, feedback) = authentication_feedback(name, result, reconnect);
            settled(&runtime, profile, generation, outcome, feedback)
        });
        let Err(refusal) = started else {
            return;
        };
        let error = match refusal {
            AuthenticationRefusal::Busy => AUTHENTICATION_BUSY.to_owned(),
            AuthenticationRefusal::Invalid(error) => error.to_string(),
        };
        self.settle_menu(
            generation,
            McpMenuOutcome::Failed,
            format!("MCP operation failed: {error}"),
        );
    }
}

fn snapshot(runtime: &McpRuntime, profile_config: Option<String>) -> McpMenuSnapshot {
    McpMenuSnapshot {
        profile_config,
        ..runtime.menu_snapshot()
    }
}

pub(super) fn settled(
    runtime: &McpRuntime,
    profile_config: Option<String>,
    generation: u64,
    outcome: McpMenuOutcome,
    feedback: String,
) -> UiEvent {
    UiEvent::McpMenuSettled {
        settled: McpMenuSettled {
            generation,
            outcome,
            feedback,
        },
        snapshot: snapshot(runtime, profile_config),
    }
}

pub(super) fn reload_feedback(
    result: Result<ReloadOutcome, ReloadFailure>,
) -> Option<(McpMenuOutcome, String)> {
    Some(match result {
        Ok(ReloadOutcome::Published { healthy: true, .. }) => {
            (McpMenuOutcome::Succeeded, RELOADED.to_owned())
        }
        Ok(ReloadOutcome::Published { unavailable, .. }) => {
            let plural = if unavailable.len() == 1 { "" } else { "s" };
            (
                McpMenuOutcome::Succeeded,
                format!(
                    "MCP reloaded with {} unavailable server{plural}.",
                    unavailable.len()
                ),
            )
        }
        Ok(ReloadOutcome::RetainedRequiredFailure(_)) => {
            (McpMenuOutcome::Failed, RETAINED.to_owned())
        }
        Err(ReloadFailure::Cancelled) => return None,
        Err(ReloadFailure::AuthorityReduced) => (
            McpMenuOutcome::Failed,
            format!("MCP reload failed: {AUTHORITY_REDUCED}"),
        ),
        Err(ReloadFailure::Failed(error)) => (
            McpMenuOutcome::Failed,
            format!("MCP reload failed: {error}"),
        ),
    })
}

fn authentication_feedback(
    name: &str,
    result: Result<AuthenticationOutcome, McpError>,
    reconnect: Option<McpError>,
) -> (McpMenuOutcome, String) {
    match result {
        Ok(AuthenticationOutcome::Authenticated { repaired_entries }) => {
            let authenticated = if repaired_entries == 0 {
                format!("Authenticated MCP server '{name}'.")
            } else {
                format!("Authenticated '{name}'; repaired {repaired_entries} credential entries.")
            };
            match reconnect {
                Some(error) => (
                    McpMenuOutcome::Failed,
                    format!("{authenticated} Connection failed: {error}. Check /mcp list."),
                ),
                None => (McpMenuOutcome::Succeeded, authenticated),
            }
        }
        Ok(AuthenticationOutcome::IssuerMismatch(_)) => (
            McpMenuOutcome::Failed,
            format!("MCP authentication for '{name}' rejected an issuer mismatch."),
        ),
        Err(error) => (
            McpMenuOutcome::Failed,
            format!(
                "MCP authentication for '{name}' failed: {}",
                authentication_error_message(&error)
            ),
        ),
    }
}

mod catalogs;
mod changes;
mod previews;

#[cfg(test)]
mod tests;
