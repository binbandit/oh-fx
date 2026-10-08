use super::turn_log::{Logged, MemoryLog, logged};
use super::*;

const CONNECTIVITY: ModelRecoveryAction = ModelRecoveryAction::WaitingForConnectivity;

fn checkpoint(progress: RecoveryProgress, consumed_attempts: usize) -> Logged {
    Logged::Recovery {
        user: "go".to_owned(),
        steps: Vec::new(),
        files: Vec::new(),
        source: String::new(),
        tool_state: RecoveryToolState::None,
        progress,
        consumed_attempts,
        fast_mode: false,
    }
}

fn lost() -> Script {
    Script::Fail(
        Vec::new(),
        failure(ProviderErrorKind::ConnectivityLost, "ConnectionFailed"),
    )
}

async fn pause_on(
    agent: &mut Agent,
    prompt: &str,
    when: impl Fn(&UiEvent) -> bool + Sync,
) -> (TurnReport, Vec<UiEvent>) {
    let pause = agent.recovery_pause();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let mut events = Vec::new();
    let report = agent
        .run_turn(
            prompt,
            &mut |event| {
                if when(&event) {
                    pause.request();
                    trigger.cancel();
                }
                events.push(event);
            },
            &cancel,
        )
        .await;
    (report, events)
}

fn waiting(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::Recovery { status, .. }
            if status.action == Some(ModelRecoveryAction::WaitingForConnectivity)
                && status.retry_wait.is_some()
    )
}

#[tokio::test(start_paused = true)]
async fn pausing_a_connectivity_wait_ends_the_turn_with_a_paused_status() {
    let provider = FakeProvider::new(vec![lost(), text_reply("next")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = pause_on(&mut agent, "go", waiting).await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(report.failure.unwrap().code(), "RecoveryPaused");
    assert_eq!(provider.requests().len(), 1);
    let paused = recoveries(&events).pop().unwrap();
    assert!(paused.is_paused());
    assert_eq!(
        paused.label(),
        "⚠ Connection lost · ConnectionFailed · recovery paused after 1 attempt"
    );
    assert!(matches!(
        events.last(),
        Some(UiEvent::TurnFinished {
            outcome: TurnOutcome::Failed,
            ..
        })
    ));
    run(&mut agent, "again").await;
    assert_eq!(
        provider.requests()[1].messages,
        vec![ChatMessage::user("again")]
    );
}

#[tokio::test(start_paused = true)]
async fn a_paused_turn_keeps_its_checkpoint_and_leaves_the_log_and_the_history() {
    let provider = FakeProvider::new(vec![lost(), text_reply("next")]);
    let (log, entries) = MemoryLog::shared();
    let shared: Arc<FakeProvider> = Arc::clone(&provider);
    let mut agent = logged(new_agent(shared, Vec::new()), log);
    pause_on(&mut agent, "go", waiting).await;
    assert_eq!(
        *entries.lock().unwrap(),
        [
            checkpoint(RecoveryProgress::Waiting(CONNECTIVITY), 1),
            checkpoint(RecoveryProgress::Paused, 1),
        ]
    );
    run(&mut agent, "again").await;
    assert_eq!(
        provider.requests()[1].messages,
        vec![ChatMessage::user("again")]
    );
}

#[tokio::test(start_paused = true)]
async fn pausing_a_retried_request_in_flight_counts_it() {
    let provider = FakeProvider::new(vec![lost(), Script::WaitForCancel]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let in_flight = |event: &UiEvent| {
        matches!(
            event,
            UiEvent::Recovery { status, .. } if status.failed_attempt == 2 && status.retry_wait.is_none()
        )
    };
    let (report, events) = pause_on(&mut agent, "go", in_flight).await;
    assert_eq!(report.failure.unwrap().code(), "RecoveryPaused");
    assert_eq!(
        recoveries(&events).pop().unwrap().label(),
        "⚠ Connection lost · Cancelled · recovery paused after 2 attempts"
    );
}

#[tokio::test(start_paused = true)]
async fn a_paused_turn_leaves_the_history_with_the_text_its_retried_request_streamed() {
    for saved in [true, false] {
        let provider = FakeProvider::new(vec![
            lost(),
            Script::StreamThenWait(vec![StreamEvent::TextDelta {
                text: "Half an answer".to_owned(),
            }]),
            text_reply("next"),
        ]);
        let (log, entries) = MemoryLog::shared();
        let shared: Arc<FakeProvider> = Arc::clone(&provider);
        let mut agent = new_agent(shared, Vec::new());
        if saved {
            agent = logged(agent, log);
        }
        let streamed = |event: &UiEvent| matches!(event, UiEvent::AssistantText { .. });
        let (report, _) = pause_on(&mut agent, "go", streamed).await;
        assert_eq!(report.failure.unwrap().code(), "RecoveryPaused", "{saved}");
        if saved {
            assert_eq!(
                *entries.lock().unwrap(),
                [
                    checkpoint(RecoveryProgress::Waiting(CONNECTIVITY), 1),
                    Logged::Recovery {
                        user: "go".to_owned(),
                        steps: Vec::new(),
                        files: Vec::new(),
                        source: "Half an answer".to_owned(),
                        tool_state: RecoveryToolState::None,
                        progress: RecoveryProgress::Paused,
                        consumed_attempts: 2,
                        fast_mode: false,
                    },
                ]
            );
        }
        run(&mut agent, "again").await;
        assert_eq!(
            provider.requests()[2].messages,
            vec![ChatMessage::user("again")],
            "{saved}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_pause_outside_a_recovery_interrupts_the_turn() {
    let provider = FakeProvider::new(vec![Script::WaitForCancel]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = pause_on(&mut agent, "go", |event| {
        matches!(event, UiEvent::TurnStarted { .. })
    })
    .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(recoveries(&events).is_empty());
    let provider = FakeProvider::new(vec![lost(), text_reply("next")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let report = run(&mut agent, "go").await.0;
    assert_eq!(report.outcome, TurnOutcome::Completed);
}

#[tokio::test(start_paused = true)]
async fn a_paused_restart_saves_the_interrupted_reply_and_tool_state() {
    let provider = FakeProvider::new(vec![Script::Fail(
        vec![
            StreamEvent::TextDelta {
                text: "Hel".to_owned(),
            },
            StreamEvent::ToolCallStarted {
                call_id: ToolCallId::new("call-1"),
                tool_name: "echo".to_owned(),
            },
        ],
        failure(ProviderErrorKind::TransportInterrupted, "RequestFailed"),
    )]);
    let (log, entries) = MemoryLog::shared();
    let shared: Arc<FakeProvider> = Arc::clone(&provider);
    let mut agent = logged(new_agent(shared, vec![echo_tool()]), log);
    let retrying = |event: &UiEvent| matches!(event, UiEvent::Recovery { status, .. } if status.retry_wait.is_some());
    pause_on(&mut agent, "go", retrying).await;
    let paused = entries
        .lock()
        .unwrap()
        .iter()
        .find(|entry| {
            matches!(
                entry,
                Logged::Recovery {
                    progress: RecoveryProgress::Paused,
                    ..
                }
            )
        })
        .cloned();
    assert_eq!(
        paused,
        Some(Logged::Recovery {
            user: "go".to_owned(),
            steps: Vec::new(),
            files: Vec::new(),
            source: "Hel".to_owned(),
            tool_state: RecoveryToolState::ProvenUnexecuted,
            progress: RecoveryProgress::Paused,
            consumed_attempts: 1,
            fast_mode: false,
        })
    );
}
