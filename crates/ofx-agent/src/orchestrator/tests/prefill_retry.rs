use super::turn_log::{Logged, MemoryLog, logged};
use super::*;

const PREFILL_REJECTION: &str = "API request failed · HTTP 400 · AI_APICallError: This model does not support assistant message prefill. The conversation must end with a user message.";
const CONTINUATION: &str = "Continue from the preceding tool result.";

fn prefill_rejected() -> Script {
    let mut error = http_failure(ProviderErrorKind::InvalidRequest, "BadRequest", 400);
    error.detail = Some(PREFILL_REJECTION.to_owned());
    Script::Fail(Vec::new(), error)
}

#[tokio::test]
async fn a_prefill_rejection_after_a_tool_result_asks_again_with_the_upstream_continuation() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        prefill_rejected(),
        text_reply("Recovered."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "Recovered.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(matches!(
        requests[1].messages.last(),
        Some(ChatMessage::Tool { .. })
    ));
    assert_eq!(
        requests[2].messages[..requests[1].messages.len()],
        requests[1].messages[..]
    );
    assert_eq!(
        requests[2].messages.last(),
        Some(&ChatMessage::user(CONTINUATION))
    );
    assert!(recoveries(&events).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_prefill_rejection_of_a_retried_request_keeps_its_recovery() {
    let mut unavailable = http_failure(ProviderErrorKind::ServerError, "server_error", 500);
    unavailable.diagnostic = Some("HTTP 500 · boom".to_owned());
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        Script::Fail(Vec::new(), unavailable.clone()),
        prefill_rejected(),
        Script::Fail(Vec::new(), unavailable),
        text_reply("Recovered."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let started = Instant::now();
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(started.elapsed(), Duration::from_millis(500));
    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    for request in &requests[3..] {
        assert_eq!(
            request.messages.last(),
            Some(&ChatMessage::user(CONTINUATION))
        );
    }
    let labels: Vec<String> = recoveries(&events)
        .iter()
        .map(RouteRecoveryStatus::label)
        .collect();
    assert_eq!(
        labels,
        [
            "⚠ Provider unavailable · HTTP 500 · boom · retrying request",
            "⚠ Provider unavailable · HTTP 500 · boom · retrying request",
            "⚠ Provider unavailable · HTTP 500 · boom · retrying request",
            "⚠ Provider unavailable · HTTP 500 · boom · retrying request",
            "✓ recovered · succeeded on attempt 4",
        ]
    );
}

#[tokio::test]
async fn the_continuation_stays_in_the_turn_without_being_saved() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        prefill_rejected(),
        tool_reply(&[("call-2", "{}")]),
        text_reply("Done."),
    ]);
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(new_agent(Arc::clone(&provider), vec![echo_tool()]), log);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let rejected = requests[1].messages.len();
    assert_eq!(
        requests[3].messages[rejected],
        ChatMessage::user(CONTINUATION)
    );
    let entries = entries.lock().unwrap();
    let [
        Logged::Turn {
            steps, steering, ..
        },
    ] = &entries[..]
    else {
        panic!("{entries:?}");
    };
    assert_eq!(steps.len(), 2);
    assert!(steering.is_empty());
}

#[tokio::test]
async fn a_second_prefill_rejection_fails_the_turn() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        prefill_rejected(),
        prefill_rejected(),
        text_reply("must not be requested"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn a_prefill_rejection_without_a_tool_result_tail_fails_the_turn() {
    let provider = FakeProvider::new(vec![
        prefill_rejected(),
        text_reply("must not be requested"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn a_rejection_after_streamed_text_is_not_retried_with_the_continuation() {
    let mut error = http_failure(ProviderErrorKind::InvalidRequest, "BadRequest", 400);
    error.detail = Some(PREFILL_REJECTION.to_owned());
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        Script::Fail(
            vec![StreamEvent::TextDelta {
                text: "Partial".to_owned(),
            }],
            error,
        ),
        text_reply("must not be requested"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(provider.requests().len(), 2);
}
