use super::*;
use crate::{PermissionMode, ToolSpec};

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: name.to_owned(),
        input_schema: "{}".into(),
    }
}

static ASK_AND_CODE: [ModeSpec; 2] = [
    ModeSpec {
        id: "ask",
        name: "Ask",
        description: "",
        permission_mode: PermissionMode::Ask,
        tool_policy: ToolPolicy::Full,
        tool_policy_denial_message: None,
    },
    ModeSpec {
        id: "code",
        name: "Code",
        description: "",
        permission_mode: PermissionMode::Auto,
        tool_policy: ToolPolicy::Full,
        tool_policy_denial_message: None,
    },
];

static FULL_AND_INSPECT: [ModeSpec; 2] = [
    ModeSpec {
        id: "full",
        name: "Full",
        description: "",
        permission_mode: PermissionMode::Ask,
        tool_policy: ToolPolicy::Full,
        tool_policy_denial_message: None,
    },
    ModeSpec {
        id: "inspect",
        name: "Inspect",
        description: "",
        permission_mode: PermissionMode::Ask,
        tool_policy: ToolPolicy::ReadOnly,
        tool_policy_denial_message: Some("Inspection mode blocks mutations."),
    },
];

static WITHOUT_MESSAGE: [ModeSpec; 1] = [ModeSpec {
    id: "inspect",
    name: "Inspect",
    description: "",
    permission_mode: PermissionMode::Ask,
    tool_policy: ToolPolicy::ReadOnly,
    tool_policy_denial_message: None,
}];

#[test]
fn mode_registry_looks_up_modes_by_id() {
    let registry = ModeRegistry {
        default_mode_id: "ask",
        modes: &ASK_AND_CODE,
    };
    assert_eq!(registry.default_mode_id, "ask");
    let found = registry.lookup("code").unwrap();
    assert_eq!(found.name, "Code");
    assert_eq!(found.permission_mode, PermissionMode::Auto);
    assert!(registry.lookup("missing").is_none());
}

#[test]
fn mode_registry_applies_tool_policy_to_the_supplied_tool_set() {
    let registry = ModeRegistry {
        default_mode_id: "full",
        modes: &FULL_AND_INSPECT,
    };
    let specs = [spec("inspect"), spec("mutate")];
    let set = ToolSet {
        specs: &specs,
        read_only_tool_names: &["inspect"],
    };
    assert!(registry.tool_allowed(&set, "full", "mutate"));
    assert!(registry.tool_allowed(&set, "inspect", "inspect"));
    assert!(!registry.tool_allowed(&set, "inspect", "mutate"));
    assert!(registry.tool_allowed(&set, "missing", "mutate"));
    assert!(registry.tool_allowed(&set, "inspect", "dynamic_tool"));

    let denied = registry
        .tool_policy_denied_json(&set, "inspect", "mutate")
        .unwrap();
    assert_eq!(
        denied,
        r#"{"error":{"type":"tool_execution_failed","tool_name":"mutate","message":"Inspection mode blocks mutations.","suggestion":"Do not retry the same tool call unchanged. Adjust the request or use an allowed alternative."}}"#
    );
    assert_eq!(
        registry.tool_policy_denied_json(&set, "inspect", "inspect"),
        None
    );
    assert_eq!(
        registry.tool_policy_denied_json(&set, "full", "mutate"),
        None
    );
}

#[test]
fn a_read_only_mode_without_its_own_message_blocks_with_the_default_reason() {
    let registry = ModeRegistry {
        default_mode_id: "inspect",
        modes: &WITHOUT_MESSAGE,
    };
    let specs = [spec("mutate")];
    let set = ToolSet {
        specs: &specs,
        read_only_tool_names: &[],
    };
    let denied = registry
        .tool_policy_denied_json(&set, "inspect", "mutate")
        .unwrap();
    assert!(denied.contains(r#""message":"Tool blocked by the active mode policy.""#));
}
