use std::sync::Arc;

use ofx_contract::{McpMenuAddForm, McpMenuAddTransport, McpMenuOutcome, McpMenuTrust};
use ofx_mcp::{
    LogoutResult, McpError, ProjectMcpAction, add_profile_server, parse_add_intent,
    remove_profile_server,
};

use super::super::{McpHost, profile_error_name};
use super::settled;
use crate::mcp_commands::{TrustSaved, save_project_action};

const SAVED: &str = "Saved MCP server; reconnecting…";
const REMOVED: &str = "Removed MCP server; reconnecting…";
const TRUST_UPDATED: &str = "Updated project MCP trust; reconnecting…";
const TRUST_UNCERTAIN: &str =
    "Project MCP choices may have been saved; live MCP authority was retired.";
const HOME_NOT_SET: &str = "HomeNotSet";
const NOT_IN_PROFILE: &str = "McpProfileServerNotFound";

impl McpHost {
    pub(super) fn add_from_menu(&self, generation: u64, form: &McpMenuAddForm) {
        let Some(path) = self.sources.profile_path() else {
            return self.fail_menu(generation, HOME_NOT_SET);
        };
        let mut tokens = Vec::new();
        match form.transport {
            McpMenuAddTransport::Local => {
                tokens.extend([form.name.as_str(), form.target.as_str()]);
                tokens.extend(
                    form.arguments
                        .split([' ', '\t'])
                        .filter(|token| !token.is_empty()),
                );
            }
            McpMenuAddTransport::Http => {
                tokens.extend(["--transport", "http", &form.name, &form.target]);
            }
        }
        let intent = match parse_add_intent(&tokens) {
            Ok(intent) => intent,
            Err(error) => return self.fail_menu(generation, &error.to_string()),
        };
        match add_profile_server(&path, intent) {
            Ok(_) => self.reload_after_change(generation, SAVED),
            Err(error) => self.fail_menu(generation, &profile_error_name(error)),
        }
    }

    pub(super) fn remove_from_menu(&self, generation: u64, server: &str) {
        let Some(path) = self.sources.profile_path() else {
            return self.fail_menu(generation, HOME_NOT_SET);
        };
        match remove_profile_server(&path, server) {
            Ok(outcome) if outcome.removed => self.reload_after_change(generation, REMOVED),
            Ok(_) => self.fail_menu(generation, NOT_IN_PROFILE),
            Err(error) => self.fail_menu(generation, &profile_error_name(error)),
        }
    }

    pub(super) fn trust_from_menu(&self, generation: u64, change: McpMenuTrust) {
        let action = match change {
            McpMenuTrust::Approve(name) => ProjectMcpAction::Approve(name),
            McpMenuTrust::Reject(name) => ProjectMcpAction::Reject(name),
            McpMenuTrust::ApproveAll => ProjectMcpAction::ApproveAll,
            McpMenuTrust::Reset => ProjectMcpAction::Reset,
        };
        match save_project_action(self, &action) {
            TrustSaved::Applied { reducing: true, .. } => {
                self.continue_menu(generation, TRUST_UPDATED);
                self.reduce_from_menu(generation, true);
            }
            TrustSaved::Applied {
                committed: true, ..
            } => self.reload_after_change(generation, TRUST_UPDATED),
            TrustSaved::Applied { .. } => {
                self.settle_menu(
                    generation,
                    McpMenuOutcome::Returned,
                    TRUST_UPDATED.to_owned(),
                );
            }
            TrustSaved::Uncertain => {
                self.continue_menu(generation, TRUST_UNCERTAIN);
                self.reduce_from_menu(generation, false);
            }
            TrustSaved::Refused(name) => self.fail_menu(generation, &name),
            TrustSaved::HomeUnavailable => self.fail_menu(generation, HOME_NOT_SET),
        }
    }

    pub(super) fn begin_menu_logout(&self, generation: u64, server: String) {
        let runtime = Arc::clone(&self.runtime);
        let emit = Arc::clone(&self.emit);
        let profile = self.profile_config();
        tokio::spawn(async move {
            let (outcome, feedback) =
                logout_feedback(&server, runtime.logout_server(&server).await);
            emit(settled(&runtime, profile, generation, outcome, feedback));
        });
    }

    fn reload_after_change(&self, generation: u64, feedback: &str) {
        self.continue_menu(generation, feedback);
        self.begin_menu_reload(generation);
    }

    fn reduce_from_menu(&self, generation: u64, rebuild: bool) {
        self.reduce_with(rebuild, self.menu_settle(generation));
    }

    fn continue_menu(&self, generation: u64, feedback: &str) {
        self.settle_menu(generation, McpMenuOutcome::Continuing, feedback.to_owned());
    }

    fn fail_menu(&self, generation: u64, error: &str) {
        self.settle_menu(
            generation,
            McpMenuOutcome::Failed,
            format!("MCP operation failed: {error}"),
        );
    }
}

fn logout_feedback(name: &str, result: Result<LogoutResult, McpError>) -> (McpMenuOutcome, String) {
    match result {
        Ok(result) if !result.removed => (
            McpMenuOutcome::Returned,
            format!("No stored MCP credentials found for '{name}'."),
        ),
        Ok(result) if result.revocation_failed => (
            McpMenuOutcome::Returned,
            format!("Logged out of '{name}' locally; remote revocation failed."),
        ),
        Ok(_) => (
            McpMenuOutcome::Returned,
            format!(
                "Logged out of MCP server '{name}'. Run /mcp auth {name} --open to sign in again."
            ),
        ),
        Err(error) => (
            McpMenuOutcome::Failed,
            format!("MCP action failed: {error}"),
        ),
    }
}

#[cfg(test)]
mod tests;
