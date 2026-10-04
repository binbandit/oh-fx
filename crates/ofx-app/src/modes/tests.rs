use ofx_contract::{ModeSpec, PermissionMode, ToolPolicy, ToolSet, ToolSpec};

use super::default_mode;

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: name.to_owned(),
        input_schema: "{}".into(),
    }
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
fn built_in_modes_allow_every_tool_of_the_supplied_tool_set() {
    let specs = [spec("read_file"), spec("write_file")];
    let set = ToolSet {
        specs: &specs,
        read_only_tool_names: &["read_file"],
    };
    let registry = default_mode().registry;
    for id in ["code", "ask"] {
        for name in ["read_file", "write_file"] {
            assert!(registry.tool_allowed(&set, id, name), "{id} {name}");
            assert_eq!(registry.tool_policy_denied_json(&set, id, name), None);
        }
    }
}
