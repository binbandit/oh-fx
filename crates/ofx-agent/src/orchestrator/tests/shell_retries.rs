use super::*;
use crate::worker_runtime::{QueuedPrompt, WorkerRuntime};

const FAILING: &str = r#"{"command":"cargo test","fail":1}"#;
const OTHER_FAILING: &str = r#"{"command":"cargo build","fail":1}"#;

struct ShellTool {
    inner: Arc<dyn Tool>,
    spec: ToolSpec,
}

impl Tool for ShellTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        self.inner.prepare(arguments)
    }
}

fn shell_tool() -> Arc<dyn Tool> {
    Arc::new(ShellTool {
        inner: echo_tool(),
        spec: ToolSpec {
            name: "shell".to_owned(),
            description: "Run a command.".to_owned(),
            input_schema: r#"{"type":"object"}"#,
        },
    })
}

fn shell_reply(calls: &[(&str, &str)]) -> Script {
    let calls = calls
        .iter()
        .map(|(id, arguments)| ToolCall {
            id: ToolCallId::new(*id),
            name: "shell".to_owned(),
            arguments: (*arguments).to_owned(),
        })
        .collect();
    Script::Reply(Vec::new(), completion(None, calls, FinishReason::ToolCalls))
}

fn operational(events: &[UiEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::Operational { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn identical_shell_failures_in_consecutive_batches_stop_the_turn_with_the_upstream_notice() {
    let provider = FakeProvider::new(vec![
        shell_reply(&[("call-1", FAILING)]),
        shell_reply(&[("call-2", FAILING)]),
        text_reply("must not be requested"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![shell_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure,
        Some(TurnFailure::RepeatedShellExecutionFailure)
    );
    assert_eq!(
        report.failure.as_ref().map(TurnFailure::code),
        Some("RepeatedShellExecutionFailure")
    );
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(
        operational(&events),
        [format!("{REPEATED_SHELL_EXECUTION_FAILURE_NOTICE}\n")]
    );
    assert_eq!(
        agent.history.last(),
        Some(&ChatMessage::Assistant {
            content: Some(REPEATED_SHELL_EXECUTION_FAILURE_NOTICE.to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        })
    );
}

#[tokio::test]
async fn a_failure_repeated_alongside_other_calls_still_stops_after_the_whole_batch() {
    let provider = FakeProvider::new(vec![
        shell_reply(&[("call-1", FAILING), ("call-2", r#"{"command":"ls"}"#)]),
        shell_reply(&[("call-3", r#"{"command":"pwd"}"#), ("call-4", FAILING)]),
        text_reply("must not be requested"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![shell_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(
        report.failure,
        Some(TurnFailure::RepeatedShellExecutionFailure)
    );
    assert_eq!(
        finished(&events),
        [
            ("call-1", ToolResultStatus::Failure),
            ("call-2", ToolResultStatus::Success),
            ("call-3", ToolResultStatus::Success),
            ("call-4", ToolResultStatus::Failure),
        ]
    );
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn shell_failures_that_change_or_skip_a_batch_keep_the_loop_running() {
    let provider = FakeProvider::new(vec![
        shell_reply(&[("call-1", FAILING)]),
        shell_reply(&[("call-2", OTHER_FAILING)]),
        shell_reply(&[("call-3", r#"{"command":"ls"}"#)]),
        shell_reply(&[("call-4", OTHER_FAILING)]),
        tool_reply(&[("call-5", r#"{"fail":1}"#)]),
        tool_reply(&[("call-6", r#"{"fail":1}"#)]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![shell_tool(), echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "done");
    assert_eq!(provider.requests().len(), 7);
}

#[tokio::test]
async fn shell_calls_rejected_before_running_never_count_as_execution_failures() {
    let provider = FakeProvider::new(vec![
        shell_reply(&[("call-1", r#"{"invalid":1}"#)]),
        shell_reply(&[("call-2", r#"{"invalid":1}"#)]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![shell_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn the_count_starts_again_in_the_next_turn() {
    let provider = FakeProvider::new(vec![
        shell_reply(&[("call-1", FAILING)]),
        text_reply("first"),
        shell_reply(&[("call-2", FAILING)]),
        text_reply("second"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![shell_tool()]);
    let (first, _) = run(&mut agent, "go").await;
    assert_eq!(first.final_text, "first");
    let (second, _) = run(&mut agent, "again").await;
    assert_eq!(second.outcome, TurnOutcome::Completed);
    assert_eq!(second.final_text, "second");
}

#[tokio::test]
async fn steering_waiting_when_the_failures_repeat_continues_the_turn() {
    let provider = FakeProvider::new(vec![
        shell_reply(&[("call-1", FAILING)]),
        shell_reply(&[("call-2", FAILING)]),
        text_reply("Steered."),
    ]);
    let worker = Arc::new(WorkerRuntime::default());
    let mut agent =
        new_agent(Arc::clone(&provider), vec![shell_tool()]).with_steering(Arc::clone(&worker));
    worker.admit(QueuedPrompt::new(0, "go".to_owned(), Vec::new()));
    let prompt = worker.take_next().expect("a queued prompt");
    let mut events = Vec::new();
    let report = agent
        .run_turn(
            &prompt.text,
            &mut |event| {
                if let UiEvent::ToolStarted { call_id, .. } = &event
                    && call_id.as_str() == "call-2"
                {
                    worker.admit(QueuedPrompt::new(
                        1,
                        "try another way".to_owned(),
                        Vec::new(),
                    ));
                }
                events.push(event);
            },
            &CancellationToken::new(),
        )
        .await;
    worker.finish_processing();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "Steered.");
    assert_eq!(provider.requests().len(), 3);
    assert!(operational(&events).is_empty());
}
