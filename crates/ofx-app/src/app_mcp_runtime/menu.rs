use std::sync::Arc;

use ofx_contract::{McpMenuRequest, McpMenuSettled, McpMenuSnapshot, UiEvent};
use ofx_mcp::{
    AuthenticationOutcome, McpError, McpRuntime, ReloadOutcome, authentication_error_message,
};

use super::{AuthenticationRefusal, McpHost, ReloadFailure, reload};

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
        }
    }

    fn profile_config(&self) -> Option<String> {
        self.sources
            .profile_path()
            .map(|path| path.display().to_string())
    }

    fn begin_menu_reload(&self, generation: u64) {
        self.discard_authentication("reload");
        let runtime = Arc::clone(&self.runtime);
        let sources = self.sources.clone();
        let reported = Arc::clone(&self.runtime);
        let profile = self.profile_config();
        self.replace_pending(
            |cancel| async move { reload(&runtime, &sources, &cancel).await },
            move |result| {
                let (succeeded, feedback) = reload_feedback(result)?;
                Some(settled(&reported, profile, generation, succeeded, feedback))
            },
        );
    }

    fn begin_menu_authentication(&self, generation: u64, server: &str) {
        let runtime = Arc::clone(&self.runtime);
        let profile = self.profile_config();
        let started = self.start_authentication(server, move |name, result, reconnect| {
            let (succeeded, feedback) = authentication_feedback(name, result, reconnect);
            settled(&runtime, profile, generation, succeeded, feedback)
        });
        let Err(refusal) = started else {
            return;
        };
        let error = match refusal {
            AuthenticationRefusal::Busy => AUTHENTICATION_BUSY.to_owned(),
            AuthenticationRefusal::Invalid(error) => error.to_string(),
        };
        (self.emit)(settled(
            &self.runtime,
            self.profile_config(),
            generation,
            false,
            format!("MCP operation failed: {error}"),
        ));
    }
}

fn snapshot(runtime: &McpRuntime, profile_config: Option<String>) -> McpMenuSnapshot {
    McpMenuSnapshot {
        profile_config,
        ..runtime.menu_snapshot()
    }
}

fn settled(
    runtime: &McpRuntime,
    profile_config: Option<String>,
    generation: u64,
    succeeded: bool,
    feedback: String,
) -> UiEvent {
    UiEvent::McpMenuSettled {
        settled: McpMenuSettled {
            generation,
            succeeded,
            feedback,
        },
        snapshot: snapshot(runtime, profile_config),
    }
}

fn reload_feedback(result: Result<ReloadOutcome, ReloadFailure>) -> Option<(bool, String)> {
    Some(match result {
        Ok(ReloadOutcome::Published { healthy: true, .. }) => (true, RELOADED.to_owned()),
        Ok(ReloadOutcome::Published { unavailable, .. }) => {
            let plural = if unavailable.len() == 1 { "" } else { "s" };
            (
                true,
                format!(
                    "MCP reloaded with {} unavailable server{plural}.",
                    unavailable.len()
                ),
            )
        }
        Ok(ReloadOutcome::RetainedRequiredFailure(_)) => (false, RETAINED.to_owned()),
        Err(ReloadFailure::Cancelled) => return None,
        Err(ReloadFailure::AuthorityReduced) => {
            (false, format!("MCP reload failed: {AUTHORITY_REDUCED}"))
        }
        Err(ReloadFailure::Failed(error)) => (false, format!("MCP reload failed: {error}")),
    })
}

fn authentication_feedback(
    name: &str,
    result: Result<AuthenticationOutcome, McpError>,
    reconnect: Option<McpError>,
) -> (bool, String) {
    match result {
        Ok(AuthenticationOutcome::Authenticated { repaired_entries }) => {
            let authenticated = if repaired_entries == 0 {
                format!("Authenticated MCP server '{name}'.")
            } else {
                format!("Authenticated '{name}'; repaired {repaired_entries} credential entries.")
            };
            match reconnect {
                Some(error) => (
                    false,
                    format!("{authenticated} Connection failed: {error}. Check /mcp list."),
                ),
                None => (true, authenticated),
            }
        }
        Ok(AuthenticationOutcome::IssuerMismatch(_)) => (
            false,
            format!("MCP authentication for '{name}' rejected an issuer mismatch."),
        ),
        Err(error) => (
            false,
            format!(
                "MCP authentication for '{name}' failed: {}",
                authentication_error_message(&error)
            ),
        ),
    }
}

#[cfg(test)]
mod tests;
