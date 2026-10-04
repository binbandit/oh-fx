use std::sync::Mutex;

use ofx_contract::{
    HistoryCut, HistoryTurn, RecoveredTurn, RecoveryPoint, RecoveryProgress, RecoveryStrategy,
};

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
        unmetered(text_reply(
            "Turn 1\nIn between: Finish after the verified read and return the result.\nT1: echoed first.txt",
        )),
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

fn unavailable() -> Script {
    Script::Fail(
        Vec::new(),
        failure(ProviderErrorKind::ServerError, "server_error"),
    )
}

fn checkpoint(steps: &[&str], progress: RecoveryProgress, consumed_attempts: usize) -> Logged {
    Logged::Recovery {
        user: "go".to_owned(),
        steps: steps.iter().map(|step| (*step).to_owned()).collect(),
        progress,
        consumed_attempts,
        fast_mode: false,
    }
}

fn checkpoints(entries: &[Logged]) -> Vec<Logged> {
    entries
        .iter()
        .filter(|entry| matches!(entry, Logged::Recovery { .. } | Logged::RecoveryCleared))
        .cloned()
        .collect()
}

const RETRYING: RecoveryProgress = RecoveryProgress::Waiting(ModelRecoveryAction::RetryingRequest);
const READ_STEP: &str = r#""" replay=false calls=["call-1"] results=["call-1=echo {}:Success"]"#;

#[tokio::test(start_paused = true)]
async fn each_retry_saves_the_turn_so_far_before_it_waits() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        unavailable(),
        unavailable(),
        text_reply("done"),
    ]);
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(new_agent(Arc::clone(&provider), vec![echo_tool()]), log);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let entries = entries.lock().unwrap();
    assert_eq!(
        entries[..2],
        [
            checkpoint(&[READ_STEP], RETRYING, 1),
            checkpoint(&[READ_STEP], RETRYING, 2),
        ]
    );
    assert!(matches!(entries[2], Logged::Turn { .. }));
    assert_eq!(entries.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_spent_retry_budget_pauses_the_turn_for_a_later_continuation() {
    let provider = FakeProvider::new(
        (0..DEFAULT_MAX_PROVIDER_ATTEMPTS)
            .map(|_| unavailable())
            .collect(),
    );
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.failure.unwrap().code(), "server_error");
    let entries = checkpoints(&entries.lock().unwrap());
    assert_eq!(entries.len(), DEFAULT_MAX_PROVIDER_ATTEMPTS);
    assert_eq!(entries[0], checkpoint(&[], RETRYING, 1));
    assert_eq!(
        entries[DEFAULT_MAX_PROVIDER_ATTEMPTS - 1],
        checkpoint(&[], RecoveryProgress::Paused, DEFAULT_MAX_PROVIDER_ATTEMPTS)
    );
}

#[tokio::test(start_paused = true)]
async fn a_failure_that_is_not_retried_saves_no_checkpoint() {
    let provider = FakeProvider::new(vec![Script::Fail(
        Vec::new(),
        failure(ProviderErrorKind::InvalidRequest, "invalid_request"),
    )]);
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    run(&mut agent, "go").await;
    assert!(checkpoints(&entries.lock().unwrap()).is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_retry_wait_discards_its_checkpoint() {
    let provider = FakeProvider::new(vec![unavailable(), unavailable()]);
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if matches!(event, UiEvent::Recovery { .. }) {
                    cancel.cancel();
                }
                events.push(event);
            },
            &cancel,
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    let entries = entries.lock().unwrap();
    assert_eq!(entries[0], checkpoint(&[], RETRYING, 1));
    assert_eq!(entries[1], Logged::RecoveryCleared);
}

#[tokio::test(start_paused = true)]
async fn a_checkpoint_that_cannot_be_saved_fails_the_turn() {
    let provider = FakeProvider::new(vec![unavailable(), text_reply("never")]);
    let mut agent = logged(
        new_agent(Arc::clone(&provider), Vec::new()),
        Box::new(MemoryLog::failing("RecoveryWriteFailed")),
    );
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(report.failure.unwrap().code(), "RecoveryWriteFailed");
    assert_eq!(provider.requests().len(), 1);
}

type Recorded = Vec<(&'static str, Vec<String>)>;

#[derive(Default)]
struct SavedArguments {
    saved: Arc<Mutex<Recorded>>,
}

impl SavedArguments {
    fn note(&self, record: &'static str, turn: &HistoryTurn<'_>) {
        let arguments = turn
            .steps
            .iter()
            .flat_map(|step| step.tool_calls.iter())
            .map(|call| call.arguments.clone())
            .collect();
        self.saved.lock().unwrap().push((record, arguments));
    }
}

impl ConversationLog for SavedArguments {
    fn require_writable(&self) -> Result<(), LogFailure> {
        Ok(())
    }

    fn record_turn(&mut self, turn: &HistoryTurn<'_>) -> Result<(), LogFailure> {
        self.note("turn", turn);
        Ok(())
    }

    fn record_compaction(
        &mut self,
        _checkpoint: &str,
        _cut: HistoryCut,
        _active: Option<&HistoryTurn<'_>>,
    ) -> Result<(), LogFailure> {
        Ok(())
    }

    fn record_recovery(&self, point: &RecoveryPoint<'_>) -> Result<(), LogFailure> {
        self.note("recovery", &point.turn);
        Ok(())
    }

    fn clear_recovery(&self) -> Result<(), LogFailure> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn checkpoints_and_saved_turns_hold_each_call_in_its_saved_form() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"in_history":true}"#)]),
        unavailable(),
        text_reply("done"),
    ]);
    let log = SavedArguments::default();
    let saved = Arc::clone(&log.saved);
    let mut agent = logged(
        new_agent(Arc::clone(&provider), vec![echo_tool()]),
        Box::new(log),
    );
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let form = vec![r#"saved {"in_history":true}"#.to_owned()];
    assert_eq!(
        *saved.lock().unwrap(),
        [("recovery", form.clone()), ("turn", form)]
    );
    let ChatMessage::Assistant { tool_calls, .. } = &provider.requests()[1].messages[1] else {
        panic!("the tool step");
    };
    assert_eq!(tool_calls[0].arguments, r#"sent saved {"in_history":true}"#);
}
