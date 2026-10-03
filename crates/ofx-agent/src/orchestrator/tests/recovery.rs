use ofx_contract::{RecoveredTurn, RecoveryStrategy};

use super::compaction::{spoken_tool_reply, unmetered, windowed};
use super::turn_log::{Logged, MemoryLog, logged};
use super::*;

fn saved_step() -> Vec<ChatMessage> {
    vec![
        ChatMessage::Assistant {
            content: Some("Checking.".to_owned()),
            tool_calls: vec![echo_call("call-1", "{}")],
            provider_replay: None,
        },
        ChatMessage::Tool {
            call_id: ToolCallId::new("call-1"),
            tool_name: "echo".to_owned(),
            content: "saved output".to_owned(),
            status: ToolResultStatus::Success,
        },
    ]
}

fn recovered(strategy: RecoveryStrategy) -> RecoveredTurn {
    RecoveredTurn {
        prompt: "fix it".to_owned(),
        messages: saved_step(),
        strategy,
        fast_mode: false,
    }
}

async fn continue_turn(agent: &mut Agent, recovered: RecoveredTurn) -> (TurnReport, Vec<UiEvent>) {
    let mut events = Vec::new();
    let report = agent
        .continue_turn(
            recovered,
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await;
    (report, events)
}

fn conversation(prompt: &str) -> Vec<ChatMessage> {
    let mut messages = vec![ChatMessage::user(prompt)];
    messages.extend(saved_step());
    messages
}

#[tokio::test]
async fn a_continued_turn_resends_its_saved_steps_without_running_them_again() {
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(new_agent(Arc::clone(&provider), vec![echo_tool()]), log);
    let (report, events) =
        continue_turn(&mut agent, recovered(RecoveryStrategy::ContinueAfterTool)).await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "done");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::ToolStarted { .. })),
        "{events:?}"
    );
    let requests = provider.requests();
    let mut expected = conversation("fix it");
    expected.push(ChatMessage::user(
        "Continue from the confirmed tool result above without repeating the tool.",
    ));
    assert_eq!(requests[0].messages, expected);
    assert_eq!(requests[0].tool_choice, ToolChoice::Auto);
    assert_eq!(
        *entries.lock().unwrap(),
        [Logged::Turn {
            user: "fix it".to_owned(),
            steps: vec![
                r#""Checking." replay=false calls=["call-1"] results=["call-1=saved output:Success"]"#
                    .to_owned()
            ],
            steering: Vec::new(),
            end: r#"replied "done" replay=false"#.to_owned(),
        }]
    );
    let (_, _) = run(&mut agent, "next").await;
    let mut history = conversation("fix it");
    history.push(ChatMessage::Assistant {
        content: Some("done".to_owned()),
        tool_calls: Vec::new(),
        provider_replay: None,
    });
    history.push(ChatMessage::user("next"));
    assert_eq!(provider.requests()[1].messages, history);
}

#[tokio::test]
async fn a_reconciling_turn_offers_no_tools_on_its_first_request_only() {
    let provider = FakeProvider::new(vec![tool_reply(&[("call-2", "{}")]), text_reply("ok")]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = continue_turn(&mut agent, recovered(RecoveryStrategy::ReconcileTool)).await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    assert_eq!(requests[0].tool_choice, ToolChoice::None);
    assert!(matches!(
        requests[0].messages.last(),
        Some(ChatMessage::User { content, .. }) if content.starts_with("Reconcile the available tool evidence")
    ));
    assert_eq!(requests[1].tool_choice, ToolChoice::Auto);
    assert_eq!(requests[1].messages[..3], conversation("fix it")[..]);
    assert!(!requests[1].messages.iter().any(|message| matches!(
        message,
        ChatMessage::User { content, .. } if content.starts_with("Reconcile")
    )));
}

#[tokio::test]
async fn a_retried_request_resends_the_saved_steps_alone() {
    for (strategy, note) in [
        (RecoveryStrategy::RetryRequest, None),
        (
            RecoveryStrategy::ContinueResponse,
            Some(
                "The previous response was interrupted. Restart that response from the beginning using the completed tool results above. Do not repeat completed tool actions.",
            ),
        ),
        (
            RecoveryStrategy::RegenerateTool,
            Some(
                "The previous response ended during an incomplete tool call. fx did not execute that call. Recreate it only if it is still needed.",
            ),
        ),
    ] {
        let provider = FakeProvider::new(vec![text_reply("ok")]);
        let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
        continue_turn(&mut agent, recovered(strategy)).await;
        let mut expected = conversation("fix it");
        expected.extend(note.map(ChatMessage::user));
        assert_eq!(provider.requests()[0].messages, expected, "{strategy:?}");
    }
}

#[tokio::test]
async fn a_reconciling_turn_keeps_its_note_and_withheld_tools_across_a_preflight_compaction() {
    let big_reply = format!("OLDER_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply("Reading first.", "call-9", r#"{"value":"first.txt"}"#),
        unmetered(text_reply(&big_reply)),
        unmetered(text_reply("Turn 1\nT1: echoed first.txt")),
        unmetered(text_reply("reconciled")),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let (older, _) = run(&mut agent, "older").await;
    assert_eq!(older.outcome, TurnOutcome::Completed);
    let (report, _) = continue_turn(&mut agent, recovered(RecoveryStrategy::ReconcileTool)).await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let continued = &requests[3];
    assert!(user_note(continued).starts_with("<compacted_conversation>"));
    assert_eq!(continued.tool_choice, ToolChoice::None);
    assert!(matches!(
        continued.messages.last(),
        Some(ChatMessage::User { content, .. }) if content.starts_with("Reconcile the available tool evidence")
    ));
}

fn user_note(request: &SeenRequest) -> &str {
    match &request.messages[0] {
        ChatMessage::User { content, .. } => content,
        other => panic!("expected a user message, got {other:?}"),
    }
}
