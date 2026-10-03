use super::*;

fn run(task: &str) -> SubagentRequestInput<'_> {
    SubagentRequestInput::Run {
        task,
        model: None,
        effort: None,
    }
}

fn message_input<'a>(
    agent: &'a str,
    instructions: Option<&'a str>,
    text: &'a str,
) -> SubagentRequestInput<'a> {
    SubagentRequestInput::Message {
        agent,
        instructions,
        message: text,
        model: None,
        effort: None,
    }
}

fn persistent(phase: ChildPhase) -> ChildSnapshot {
    ChildSnapshot {
        kind: ChildKind::Persistent,
        phase,
    }
}

#[test]
fn minimal_request_validation_owns_one_off_and_persistent_intent() {
    let run = SubagentRequest::validate(run("review this")).unwrap();
    assert_eq!(run.action(), SubagentAction::Run);
    assert_eq!(run.plan(None), SubagentPlan::CreateOneOff);

    let message = SubagentRequest::validate(message_input(
        "reviewer",
        Some("Review strictly."),
        "review this",
    ))
    .unwrap();
    assert_eq!(message.action(), SubagentAction::Message);
    assert_eq!(message.plan(None), SubagentPlan::CreatePersistent);
    assert_eq!(message.instructions(), Some("Review strictly."));
    assert_eq!(
        SubagentRequest::validate(message_input("reviewer", Some(""), "review this")),
        Err(SubagentRequestError::InvalidInstructions)
    );
}

#[test]
fn persistent_instruction_updates_participate_in_operation_identity() {
    let inherited =
        SubagentRequest::validate(message_input("reviewer", None, "review this")).unwrap();
    let strict = SubagentRequest::validate(message_input(
        "reviewer",
        Some("Review strictly."),
        "review this",
    ))
    .unwrap();
    let security = SubagentRequest::validate(message_input(
        "reviewer",
        Some("Review security."),
        "review this",
    ))
    .unwrap();
    assert_ne!(inherited.fingerprint(), strict.fingerprint());
    assert_ne!(strict.fingerprint(), security.fingerprint());
}

#[test]
fn creation_overrides_validate_and_participate_in_operation_identity() {
    let plain = SubagentRequest::validate(run("review this")).unwrap();
    let routed = SubagentRequest::validate(SubagentRequestInput::Run {
        task: "review this",
        model: Some("gpt-5.6-sol-fast"),
        effort: Some("medium"),
    })
    .unwrap();
    assert_eq!(routed.overrides().model, Some("gpt-5.6-sol-fast"));
    assert_eq!(
        routed.overrides().effort.map(ReasoningEffort::label),
        Some("medium")
    );
    assert!(!plain.overrides().is_present());
    assert!(routed.overrides().is_present());
    let expected_plain: [u8; 32] =
        Sha256::digest(b"fx.subagent.request.v1\0run\0review this").into();
    assert_eq!(plain.fingerprint(), expected_plain);
    assert_ne!(plain.fingerprint(), routed.fingerprint());

    let rerouted = SubagentRequest::validate(SubagentRequestInput::Message {
        agent: "reviewer",
        instructions: None,
        message: "review this",
        model: None,
        effort: Some("high"),
    })
    .unwrap();
    assert_eq!(rerouted.overrides().model, None);
    assert_eq!(
        rerouted.overrides().effort.map(ReasoningEffort::label),
        Some("high")
    );

    assert_eq!(
        SubagentRequest::validate(SubagentRequestInput::Run {
            task: "t",
            model: Some(""),
            effort: None,
        }),
        Err(SubagentRequestError::InvalidModel)
    );
    assert_eq!(
        SubagentRequest::validate(SubagentRequestInput::Run {
            task: "t",
            model: None,
            effort: Some("not an effort!"),
        }),
        Err(SubagentRequestError::InvalidEffort)
    );
}

#[test]
fn validation_reports_the_first_invalid_field_in_upstream_order() {
    let long_task = "x".repeat(MAX_PROMPT_BYTES + 1);
    let long_model = "m".repeat(MAX_MODEL_BYTES + 1);
    for (input, error) in [
        (run(""), SubagentRequestError::InvalidTask),
        (run("a\0b"), SubagentRequestError::InvalidTask),
        (run(&long_task), SubagentRequestError::InvalidTask),
        (
            SubagentRequestInput::Run {
                task: "",
                model: Some(""),
                effort: Some("bad effort"),
            },
            SubagentRequestError::InvalidTask,
        ),
        (
            SubagentRequestInput::Run {
                task: "t",
                model: Some(&long_model),
                effort: Some("bad effort"),
            },
            SubagentRequestError::InvalidModel,
        ),
        (
            message_input("Reviewer", Some(""), ""),
            SubagentRequestError::InvalidAgent,
        ),
        (
            message_input("reviewer", Some("a\0b"), ""),
            SubagentRequestError::InvalidInstructions,
        ),
        (
            message_input("reviewer", None, ""),
            SubagentRequestError::InvalidMessage,
        ),
    ] {
        assert_eq!(SubagentRequest::validate(input), Err(error), "{input:?}");
    }
    assert_eq!(SubagentRequestError::InvalidAgent.code(), "invalid_agent");
    assert_eq!(SubagentRequestError::InvalidEffort.code(), "invalid_effort");
}

#[test]
fn persistent_planning_derives_continuation_steering_and_busy_overlay_changes() {
    let continued = SubagentRequest::validate(message_input("reviewer", None, "continue")).unwrap();
    assert_eq!(
        continued.plan(Some(persistent(ChildPhase::Idle))),
        SubagentPlan::ContinuePersistent
    );
    assert_eq!(
        continued.plan(Some(persistent(ChildPhase::Interrupted))),
        SubagentPlan::ContinuePersistent
    );
    assert_eq!(
        continued.plan(Some(persistent(ChildPhase::Running))),
        SubagentPlan::SteerPersistent
    );
    assert_eq!(
        continued.plan(Some(persistent(ChildPhase::AwaitingApproval))),
        SubagentPlan::SteerPersistent
    );
    assert_eq!(
        continued.plan(Some(persistent(ChildPhase::Finished))),
        SubagentPlan::Reject(SubagentRejectCode::ChildUnavailable)
    );
    assert_eq!(
        continued.plan(Some(ChildSnapshot {
            kind: ChildKind::OneOff,
            phase: ChildPhase::Idle,
        })),
        SubagentPlan::Reject(SubagentRejectCode::ChildNotPersistent)
    );
    let overlay =
        SubagentRequest::validate(message_input("reviewer", Some("new overlay"), "continue"))
            .unwrap();
    assert_eq!(
        overlay.plan(Some(persistent(ChildPhase::Running))),
        SubagentPlan::Reject(SubagentRejectCode::ChildBusy)
    );
    assert_eq!(SubagentRejectCode::ChildBusy.code(), "child_busy");
}

#[test]
fn terminal_result_omits_scheduler_identities_and_phases() {
    let encoded = SubagentResult {
        ok: true,
        result: Some("review complete"),
        ..SubagentResult::default()
    }
    .encode();
    assert_eq!(
        encoded,
        r#"{"ok":true,"result":"review complete","error_code":null}"#
    );
    for absent in [
        "retryable",
        "requested",
        "cursor",
        "operation_id",
        "child_id",
        "status",
    ] {
        assert!(!encoded.contains(absent), "{absent}");
    }
}

#[test]
fn results_encode_upstream_fields_in_order_with_json_escapes() {
    assert_eq!(
        SubagentResult::failure("host_unavailable").encode(),
        r#"{"ok":false,"result":null,"error_code":"host_unavailable"}"#
    );
    assert_eq!(
        SubagentResult {
            ok: true,
            pending: true,
            result: Some("line\n\"quoted\"\t\u{1b}é"),
            ..SubagentResult::default()
        }
        .encode(),
        "{\"ok\":true,\"result\":\"line\\n\\\"quoted\\\"\\t\\u001bé\",\"error_code\":null,\"pending\":true}"
    );
    let long_code = "c".repeat(70);
    assert_eq!(
        SubagentResult::failure(&long_code).encode(),
        format!(
            r#"{{"ok":false,"result":null,"error_code":"{}"}}"#,
            "c".repeat(64)
        )
    );
}

#[test]
fn results_encode_feedback_delivery_after_the_terminal_fields() {
    for (delivery, label) in [
        (SteeringDelivery::Queued, "queued"),
        (SteeringDelivery::Applied, "applied"),
        (SteeringDelivery::NotApplied, "not_applied"),
    ] {
        assert_eq!(
            SubagentResult {
                ok: true,
                delivery: Some(delivery),
                ..SubagentResult::default()
            }
            .encode(),
            format!(r#"{{"ok":true,"result":null,"error_code":null,"delivery":"{label}"}}"#)
        );
    }
}
