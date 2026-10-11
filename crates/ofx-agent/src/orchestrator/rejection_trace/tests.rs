use ofx_contract::DEFERRED_TOOL_OUTPUT;

use super::*;

#[test]
fn denials_use_upstreams_reason_tags() {
    let names = [
        Denial::User,
        Denial::ReviewCaution,
        Denial::ReviewEvidenceIncomplete,
        Denial::ReviewUnavailable,
    ]
    .map(Denial::name);
    assert_eq!(
        names,
        [
            "user_denied",
            "review_caution",
            "review_evidence_incomplete",
            "review_unavailable",
        ]
    );
}

#[test]
fn argument_integrity_and_hook_blocks_use_upstreams_tags() {
    let integrities = [
        ToolArgumentIntegrity::Valid,
        ToolArgumentIntegrity::MalformedJson,
        ToolArgumentIntegrity::NonObjectJson,
    ]
    .map(integrity_name);
    assert_eq!(integrities, ["valid", "malformed_json", "non_object_json"]);
    assert_eq!(
        [HookBlock::Blocked, HookBlock::FailedClosed].map(HookBlock::name),
        ["lifecycle_block", "lifecycle_failed_closed"]
    );
}

#[test]
fn unexecuted_calls_name_why_they_did_not_run() {
    assert_eq!(
        deferral_kind(CONTEXT_DEFERRED_TOOL_OUTPUT),
        "context_deferred"
    );
    assert_eq!(
        deferral_kind(DEFERRED_TOOL_OUTPUT),
        "applicable_targets_changed"
    );
}
