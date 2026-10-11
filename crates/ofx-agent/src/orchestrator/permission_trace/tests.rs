use std::path::PathBuf;

use ofx_contract::{FileMutation, FileMutationState};

use super::*;

fn asked<'a>(
    call: &'a ToolCall,
    action: GatedAction<'a>,
    facts: PermissionTraceFacts,
) -> Asked<'a> {
    Asked {
        context: TraceContext::default(),
        call,
        action,
        facts,
    }
}

#[test]
fn decisions_use_upstreams_names() {
    assert_eq!(
        [
            Granted::Once,
            Granted::Always,
            Granted::Deny,
            Granted::PermissionRequired,
        ]
        .map(Granted::name),
        ["once", "always", "deny", "permission_required"]
    );
    assert_eq!(
        [
            PermissionMode::Ask,
            PermissionMode::Auto,
            PermissionMode::Yolo
        ]
        .map(mode_name),
        ["ask", "auto", "yolo"]
    );
}

#[test]
fn the_approval_source_follows_upstreams_order() {
    let call = ToolCall::new("call_1", "read_file", "{}");
    let mutation = FileMutation {
        target: PathBuf::from("/work/a.txt"),
        state: FileMutationState::Changes,
    };
    let facts = |mode, local_grants| PermissionTraceFacts {
        mode,
        local_grants,
        outside_workspace: "false",
    };
    let read = GatedAction::Call(&call);
    let write = GatedAction::FileMutation(&mutation);
    let cases = [
        (
            asked(&call, read, facts(PermissionMode::Auto, 2)),
            Granted::Deny,
            "denied",
        ),
        (
            asked(&call, read, facts(PermissionMode::Ask, 0)),
            Granted::PermissionRequired,
            "denied",
        ),
        (
            asked(&call, write, facts(PermissionMode::Auto, 2)),
            Granted::Once,
            "file_mutation",
        ),
        (
            asked(&call, read, facts(PermissionMode::Ask, 1)),
            Granted::Always,
            "session_grant_or_rule",
        ),
        (
            asked(&call, read, facts(PermissionMode::Auto, 0)),
            Granted::Once,
            "auto_or_rule",
        ),
        (
            asked(&call, read, facts(PermissionMode::Ask, 0)),
            Granted::Once,
            "interactive_or_rule",
        ),
        (
            asked(&call, read, facts(PermissionMode::Yolo, 0)),
            Granted::Once,
            "yolo",
        ),
    ];
    for (asked, decision, expected) in cases {
        assert_eq!(source(&asked, decision), expected, "{decision:?}");
    }
}
