use std::path::Path;
use std::sync::Arc;

use ofx_contract::{ModeSpec, PermissionMode, Tool, ToolPolicy, ToolSet};
use ofx_tools::ReadFile;

use super::default_mode;

fn names(tools: &[Arc<dyn Tool>]) -> Vec<&str> {
    tools.iter().map(|tool| tool.spec().name.as_str()).collect()
}

#[test]
fn built_in_modes_register_exact_acp_order_and_permission_policy() {
    let registry = default_mode().registry;
    assert_eq!(
        registry.modes,
        [
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
        ]
    );
    assert_eq!(registry.default_mode_id, "ask");
    assert_eq!(default_mode().id, "ask");
    assert_eq!(
        registry.lookup("code").map(|mode| mode.permission_mode),
        Some(PermissionMode::Auto)
    );
    assert_eq!(
        registry.lookup("ask").map(|mode| mode.permission_mode),
        Some(PermissionMode::Ask)
    );
    assert_eq!(registry.lookup("unknown"), None);
}

#[test]
fn built_in_read_only_tool_set_matches_plan_inspection_tools() {
    assert_eq!(
        default_mode().read_only_tool_names,
        ["read_file", "glob_files", "grep_files"]
    );
}

#[test]
fn built_in_mode_projections_use_the_supplied_tool_set() {
    let tools: [Arc<dyn Tool>; 1] = [Arc::new(ReadFile::new(Path::new("/workspace")))];
    let set = ToolSet {
        tools: &tools,
        read_only_tool_names: &["write_file", "read_file"],
    };
    let projected = default_mode().registry.model_tools(&set, "ask");
    assert_eq!(names(&projected), ["read_file"]);
}
