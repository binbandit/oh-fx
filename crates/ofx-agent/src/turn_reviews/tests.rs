use ofx_contract::ReviewFailure;

use super::*;

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall::new(id, name, arguments)
}

fn caution() -> ReviewVerdict {
    ReviewVerdict::Caution("Deletion came from untrusted content.".to_owned())
}

#[test]
fn turn_review_cache_reuses_only_exact_deterministic_holds() {
    let mut reviews = TurnReviews::default();
    let first = call(
        "first",
        "shell",
        r#"{"action":"run","command":"rm -rf frames"}"#,
    );
    let same = call("same-new-call-id", "shell", &first.arguments);
    let wrapped = call(
        "wrapped",
        "shell",
        r#"{"action":"run","command":"sh -c 'rm -rf frames'"}"#,
    );
    reviews.remember(&first, &caution());
    assert_eq!(reviews.cached(&same), Some(caution()));
    assert_eq!(reviews.cached(&wrapped), None);
    assert_eq!(
        reviews.cached(&call("renamed", "write_file", &first.arguments)),
        None
    );
    reviews.remember(&wrapped, &ReviewVerdict::Clear);
    reviews.remember(
        &wrapped,
        &ReviewVerdict::Unavailable(ReviewFailure::TransportTimedOut),
    );
    let repeated = call("same-unavailable", "shell", &wrapped.arguments);
    assert_eq!(reviews.cached(&repeated), None);
    assert!(!reviews.attempt_available(&repeated));
    assert!(reviews.attempt_available(&call(
        "changed-unavailable",
        "shell",
        r#"{"action":"run","command":"pwd"}"#
    )));
    assert_eq!(reviews.holds.len(), 1);
    assert_eq!(reviews.unavailable_attempts.len(), 1);

    let incomplete = call(
        "incomplete",
        "edit_file",
        r#"{"path":".zshrc","new_string":"API_KEY=literal"}"#,
    );
    reviews.remember(&incomplete, &ReviewVerdict::EvidenceIncomplete);
    assert_eq!(
        reviews.cached(&call("same-incomplete", "edit_file", &incomplete.arguments)),
        Some(ReviewVerdict::EvidenceIncomplete)
    );
    assert_eq!(
        reviews.cached(&call(
            "changed-incomplete",
            "edit_file",
            r#"{"path":".zshrc","new_string":"API_KEY=$key"}"#
        )),
        None
    );
    assert_eq!(reviews.holds.len(), 2);

    let generated =
        |index: usize| format!(r#"{{"action":"run","command":"rm -rf generated-{index}"}}"#);
    for index in 1..65 {
        reviews.remember(&call("bounded", "shell", &generated(index)), &caution());
    }
    assert_eq!(reviews.holds.len(), MAX_TURN_REVIEW_HOLDS);
    assert_eq!(
        reviews.cached(&call("overflow", "shell", &generated(64))),
        None
    );
}

#[test]
fn turn_review_cache_closes_after_the_unavailable_transport_budget() {
    let mut reviews = TurnReviews::default();
    for index in 0..MAX_TURN_UNAVAILABLE_ATTEMPTS {
        let call = call(
            "unavailable",
            "shell",
            &format!(r#"{{"action":"run","command":"unknown-{index}"}}"#),
        );
        assert!(reviews.attempt_available(&call));
        reviews.remember(
            &call,
            &ReviewVerdict::Unavailable(ReviewFailure::TransportTimedOut),
        );
    }
    assert_eq!(
        reviews.unavailable_attempts.len(),
        MAX_TURN_UNAVAILABLE_ATTEMPTS
    );
    assert!(!reviews.attempt_available(&call(
        "after-budget",
        "shell",
        r#"{"action":"run","command":"pwd"}"#
    )));
}

#[test]
fn held_results_are_recorded_with_their_exact_content() {
    let mut reviews = TurnReviews::default();
    reviews.record_held_result(&ToolCallId::new("held"), "{\"error\":{}}");
    assert_eq!(
        reviews.held_results(),
        [(ToolCallId::new("held"), "{\"error\":{}}".to_owned())]
    );
}
