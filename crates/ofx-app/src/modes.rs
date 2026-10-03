use ofx_contract::{ActiveMode, ModeRegistry, ModeSpec, PermissionMode, ToolPolicy};

use crate::tool_set::READ_ONLY_TOOL_NAMES;

static BUILT_IN_MODES: [ModeSpec; 2] = [
    ModeSpec {
        id: "code",
        name: "Code",
        description: "Write and modify code with full tool access",
        permission_mode: PermissionMode::Auto,
        tool_policy: ToolPolicy::Full,
        tool_policy_denial_message: None,
    },
    ModeSpec {
        id: "ask",
        name: "Ask",
        description: "Request permission before making any changes",
        permission_mode: PermissionMode::Ask,
        tool_policy: ToolPolicy::Full,
        tool_policy_denial_message: None,
    },
];

static REGISTRY: ModeRegistry = ModeRegistry {
    default_mode_id: "ask",
    modes: &BUILT_IN_MODES,
};

pub fn default_mode() -> ActiveMode {
    ActiveMode {
        registry: &REGISTRY,
        id: REGISTRY.default_mode_id,
        read_only_tool_names: &READ_ONLY_TOOL_NAMES,
    }
}

#[cfg(test)]
mod tests;
