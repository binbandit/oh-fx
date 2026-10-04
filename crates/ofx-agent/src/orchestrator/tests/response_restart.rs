use super::*;
use crate::worker_runtime::{QueuedPrompt, WorkerRuntime};

const RESTARTED: &str = "\n\n[Response interrupted. Restarting.]\n\n";
const CONTINUE_NOTE: &str = "The previous response was interrupted. Restart that response from the beginning using the completed tool results above. Do not repeat completed tool actions.";
const RESTARTING: &str = "⚠ Network interrupted · RequestFailed · restarting response";

fn interrupted_after(text: &str) -> Script {
    let stream = if text.is_empty() {
        Vec::new()
    } else {
        vec![StreamEvent::TextDelta {
            text: text.to_owned(),
        }]
    };
    Script::Fail(
        stream,
        failure(ProviderErrorKind::TransportInterrupted, "RequestFailed"),
    )
}

fn shown(events: &[UiEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::AssistantText { text, .. } => Some(text.clone()),
            UiEvent::AssistantRestarted { text, .. } => Some(format!("restart {text:?}")),
            UiEvent::Recovery { status, .. } => Some(status.label()),
            _ => None,
        })
        .collect()
}

fn notes(requests: &[SeenRequest]) -> Vec<bool> {
    requests
        .iter()
        .map(|request| request.messages.last() == Some(&ChatMessage::user(CONTINUE_NOTE)))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_reply_restarts_from_the_beginning_with_the_upstream_note() {
    let provider = FakeProvider::new(vec![interrupted_after("Hel"), text_reply("Hello there.")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "hi").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "Hello there.");
    assert_eq!(
        shown(&events),
        [
            "Hel".to_owned(),
            RESTARTING.to_owned(),
            format!("restart {RESTARTED:?}"),
            RESTARTING.to_owned(),
            "Hello there.".to_owned(),
            "✓ recovered · succeeded on attempt 2".to_owned(),
        ]
    );
    let requests = provider.requests();
    assert_eq!(notes(&requests), [false, true]);
    assert_eq!(
        requests[1].messages[..requests[1].messages.len() - 1],
        requests[0].messages[..]
    );
    assert_eq!(
        agent.history,
        [
            ChatMessage::user("hi"),
            ChatMessage::Assistant {
                content: Some("Hello there.".to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_restart_that_fails_before_new_text_keeps_restarting_with_one_notice() {
    let provider = FakeProvider::new(vec![
        interrupted_after("Hel"),
        interrupted_after(""),
        text_reply("Hello."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "hi").await;
    assert_eq!(report.final_text, "Hello.");
    assert_eq!(notes(&provider.requests()), [false, true, true]);
    let restarts = events
        .iter()
        .filter(|event| matches!(event, UiEvent::AssistantRestarted { .. }))
        .count();
    assert_eq!(restarts, 1);
    let actions: Vec<_> = recoveries(&events)
        .iter()
        .filter_map(|status| status.action)
        .collect();
    assert_eq!(actions, [ModelRecoveryAction::ContinuingResponse; 4]);
}

#[tokio::test(start_paused = true)]
async fn the_same_partial_reply_failing_at_the_same_point_stops_the_restarts() {
    let provider = FakeProvider::new(vec![
        interrupted_after("Hel"),
        interrupted_after("Hel"),
        interrupted_after("Hel"),
        text_reply("must not be requested"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "hi").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(
        recoveries(&events).pop().unwrap().label(),
        "⚠ Network interrupted · RequestFailed · kept failing at the same point · stopped"
    );
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_restart_wait_keeps_the_interrupted_reply() {
    let provider = FakeProvider::new(vec![interrupted_after("Hel"), text_reply("never")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let cancel = CancellationToken::new();
    let report = agent
        .run_turn(
            "hi",
            &mut |event| {
                if matches!(event, UiEvent::Recovery { .. }) {
                    cancel.cancel();
                }
            },
            &cancel,
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        agent.history.last(),
        Some(&ChatMessage::Assistant {
            content: Some("Hel".to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn a_restart_after_a_tool_step_keeps_the_step_and_asks_for_the_reply_again() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        interrupted_after("Do"),
        text_reply("Done."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.final_text, "Done.");
    let requests = provider.requests();
    assert_eq!(notes(&requests), [false, false, true]);
    assert!(matches!(
        &requests[2].messages[requests[2].messages.len() - 2],
        ChatMessage::Tool { call_id, .. } if call_id.as_str() == "call-1"
    ));
}

#[tokio::test(start_paused = true)]
async fn steering_that_interrupts_a_restarted_reply_drops_the_restart_note() {
    let provider = FakeProvider::new(vec![
        interrupted_after("Hel"),
        Script::StreamThenWait(vec![StreamEvent::TextDelta {
            text: "Hello th".to_owned(),
        }]),
        text_reply("Steered."),
    ]);
    let worker = Arc::new(WorkerRuntime::default());
    let mut agent = new_agent(Arc::clone(&provider), Vec::new()).with_steering(Arc::clone(&worker));
    worker.admit(QueuedPrompt::new(0, "hi".to_owned(), Vec::new()));
    let prompt = worker.take_next().unwrap();
    let report = agent
        .run_turn(
            &prompt.text,
            &mut |event| {
                if matches!(&event, UiEvent::AssistantText { text, .. } if text == "Hello th") {
                    worker.admit(QueuedPrompt::new(1, "be brief".to_owned(), Vec::new()));
                }
            },
            &CancellationToken::new(),
        )
        .await;
    worker.finish_processing();
    assert_eq!(report.final_text, "Steered.");
    assert_eq!(notes(&provider.requests()), [false, true, false]);
}
