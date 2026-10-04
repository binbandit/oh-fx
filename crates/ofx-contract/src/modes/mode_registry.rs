use super::mode_contract::{ModeSpec, ToolPolicy};
use crate::tool_result_errors::pre_tool_use_blocked_json;
use crate::tool_set::ToolSet;

const DEFAULT_DENIAL_MESSAGE: &str = "Tool blocked by the active mode policy.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeRegistry {
    pub default_mode_id: &'static str,
    pub modes: &'static [ModeSpec],
}

impl ModeRegistry {
    pub fn lookup(&self, id: &str) -> Option<&'static ModeSpec> {
        self.modes.iter().find(|mode| mode.id == id)
    }

    pub fn tool_allowed(&self, set: &ToolSet<'_>, id: &str, tool_name: &str) -> bool {
        if !set.registers(tool_name) {
            return true;
        }
        self.lookup(id).is_none_or(|mode| match mode.tool_policy {
            ToolPolicy::Full => true,
            ToolPolicy::ReadOnly => set.is_read_only(tool_name),
        })
    }

    pub fn tool_policy_denied_json(
        &self,
        set: &ToolSet<'_>,
        id: &str,
        tool_name: &str,
    ) -> Option<String> {
        if self.tool_allowed(set, id, tool_name) {
            return None;
        }
        let mode = self.lookup(id)?;
        let reason = mode
            .tool_policy_denial_message
            .unwrap_or(DEFAULT_DENIAL_MESSAGE);
        Some(pre_tool_use_blocked_json(tool_name, reason))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveMode {
    pub registry: &'static ModeRegistry,
    pub id: &'static str,
    pub read_only_tool_names: &'static [&'static str],
}

#[cfg(test)]
mod tests;
