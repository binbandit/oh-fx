use ofx_contract::{LogFailure, ProviderError, ProviderErrorKind, Usage};

use super::*;

fn report(outcome: TurnOutcome, final_text: &str, failure: Option<TurnFailure>) -> TurnReport {
    TurnReport {
        outcome,
        final_text: final_text.to_owned(),
        usage: Usage::default(),
        failure,
    }
}

#[test]
fn subagent_failure_capture_is_bounded_and_keeps_safe_detail_only() {
    assert_eq!(
        failure_diagnostic_value("agent_turn_failed", "SessionCommitFailed").as_str(),
        "agent_turn_failed: SessionCommitFailed"
    );
    let long = failure_diagnostic_value("stage", &"界".repeat(200));
    assert!(long.as_str().len() <= MAX_DIAGNOSTIC_BYTES);
    assert!(long.as_str().starts_with("stage: 界"));
    assert_eq!(
        failure_diagnostic_value("provider_http_error", "unsafe\u{1b}[31m").as_str(),
        "provider_http_error"
    );
}

#[test]
fn subagent_finalization_preserves_every_outcome() {
    let cases = [
        (TurnOutcome::Completed, false, Outcome::Completed),
        (TurnOutcome::Failed, false, Outcome::Failed),
        (TurnOutcome::Interrupted, false, Outcome::Interrupted),
        (TurnOutcome::Completed, true, Outcome::Cancelled),
        (TurnOutcome::Failed, true, Outcome::Cancelled),
        (TurnOutcome::Interrupted, true, Outcome::Cancelled),
    ];
    for (turn, cancelled, expected) in cases {
        let outcome = work_outcome(report(turn, "", None), String::new(), cancelled);
        assert_eq!(outcome.outcome, expected, "{turn:?} {cancelled}");
        assert_eq!(outcome.failure.is_some(), expected == Outcome::Failed);
    }
}

#[test]
fn completed_work_keeps_its_reply_and_failed_work_its_partial_text() {
    let completed = work_outcome(
        report(TurnOutcome::Completed, "review complete", None),
        "streamed".to_owned(),
        false,
    );
    assert_eq!(completed.text.as_deref(), Some("review complete"));
    let failed = work_outcome(
        report(TurnOutcome::Failed, "", Some(TurnFailure::StepLimitReached)),
        "one edit completed".to_owned(),
        false,
    );
    assert_eq!(failed.text.as_deref(), Some("one edit completed"));
    assert_eq!(
        failed.failure.unwrap().as_str(),
        "agent_turn_failed: StepLimitReached"
    );
    let silent = work_outcome(
        report(TurnOutcome::Interrupted, "", None),
        String::new(),
        false,
    );
    assert_eq!(silent.text, None);
}

#[test]
fn a_reply_the_child_could_not_save_fails_the_work_and_keeps_the_reply() {
    let unsaved = LogFailure {
        code: "SessionCommitFailed".to_owned(),
    };
    for (turn, partial) in [
        (TurnOutcome::Completed, ""),
        (TurnOutcome::Interrupted, "fixed it"),
    ] {
        let outcome = work_outcome(
            report(
                turn,
                "fixed it",
                Some(TurnFailure::Persistence(unsaved.clone())),
            ),
            partial.to_owned(),
            false,
        );
        assert_eq!(outcome.outcome, Outcome::Failed, "{turn:?}");
        assert_eq!(
            outcome.failure.unwrap().as_str(),
            "agent_turn_failed: SessionCommitFailed"
        );
        assert_eq!(outcome.text.as_deref(), Some("fixed it"));
    }
}

#[test]
fn provider_http_failures_name_the_request_failure() {
    let mut error = ProviderError::new(ProviderErrorKind::Protocol, "HttpStatus");
    error.status = Some(500);
    error.diagnostic = Some("HTTP 500 · boom".to_owned());
    assert_eq!(
        turn_failure_diagnostic(Some(&TurnFailure::Provider(error.clone()))).as_str(),
        "provider_http_error: API request failed · HTTP 500 · boom"
    );
    error.status = Some(401);
    assert_eq!(
        turn_failure_diagnostic(Some(&TurnFailure::Provider(error.clone()))).as_str(),
        "provider_http_error: API access denied · HTTP 500 · boom"
    );
    error.status = None;
    assert_eq!(
        turn_failure_diagnostic(Some(&TurnFailure::Provider(error))).as_str(),
        "agent_turn_failed: HttpStatus"
    );
    assert_eq!(
        turn_failure_diagnostic(None).as_str(),
        "agent_execution: ProviderFailed"
    );
}

#[test]
fn instructions_extend_the_trusted_base_prompt_as_an_overlay() {
    assert_eq!(system_prompt("base", ""), "base");
    assert_eq!(
        system_prompt("base", "Review strictly."),
        "base\n\n<subagent_instructions>\nReview strictly.\n</subagent_instructions>"
    );
}
