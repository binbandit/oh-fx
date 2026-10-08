use ofx_contract::{RecoveredTurn, RecoveryStrategy};

use super::*;

const REGENERATE_NOTE: &str = "The previous response ended during an incomplete tool call. fx did not execute that call. Recreate it only if it is still needed.";
const RECONCILE_NOTE: &str = "Reconcile the available tool evidence above before continuing. Do not repeat the tool unless the evidence proves it is safe.";
const REGENERATING: &str = "⚠ Network interrupted · RequestFailed · regenerating unstarted tool";

fn started(name: &str) -> StreamEvent {
    StreamEvent::ToolCallStarted {
        call_id: ToolCallId::new("call-1"),
        tool_name: name.to_owned(),
    }
}

fn input(text: &str) -> StreamEvent {
    StreamEvent::ToolInputDelta {
        text: text.to_owned(),
    }
}

fn interrupted(events: Vec<StreamEvent>) -> Script {
    Script::Fail(
        events,
        failure(ProviderErrorKind::TransportInterrupted, "RequestFailed"),
    )
}

const REGENERATE: Option<&str> = Some(REGENERATE_NOTE);
const RECONCILE: Option<&str> = Some(RECONCILE_NOTE);

fn notes(provider: &FakeProvider) -> Vec<Option<&'static str>> {
    provider
        .requests()
        .iter()
        .map(|request| {
            [REGENERATE_NOTE, RECONCILE_NOTE]
                .into_iter()
                .find(|note| request.messages.last() == Some(&ChatMessage::user(*note)))
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_tool_call_cut_off_by_a_network_failure_is_regenerated_with_the_upstream_note() {
    let provider = FakeProvider::new(vec![
        interrupted(vec![started("echo"), input(r#"{"text":"#)]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.final_text, "done");
    assert_eq!(notes(&provider), [None, REGENERATE]);
    assert_eq!(provider.requests()[1].tool_choice, ToolChoice::Auto);
    let labels: Vec<String> = recoveries(&events)
        .iter()
        .map(RouteRecoveryStatus::label)
        .collect();
    assert_eq!(
        labels,
        [
            REGENERATING,
            REGENERATING,
            "✓ recovered · succeeded on attempt 2"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_tool_evidence_outlives_a_retry_that_streams_nothing() {
    let provider = FakeProvider::new(vec![
        interrupted(vec![started("echo")]),
        interrupted(Vec::new()),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.final_text, "done");
    assert_eq!(notes(&provider), [None, REGENERATE, REGENERATE]);
}

#[tokio::test(start_paused = true)]
async fn streamed_tool_input_counts_as_progress_toward_the_stall_check() {
    let advancing = FakeProvider::new(vec![
        interrupted(vec![started("echo"), input("{")]),
        interrupted(vec![started("echo"), input(r#"{"te"#)]),
        interrupted(vec![started("echo"), input(r#"{"text"#)]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&advancing), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(advancing.requests().len(), 4);
    let stalled = FakeProvider::new(vec![
        interrupted(vec![started("echo"), input("{")]),
        interrupted(vec![started("echo"), input("{")]),
        interrupted(vec![started("echo"), input("{")]),
        text_reply("must not be requested"),
    ]);
    let mut agent = new_agent(Arc::clone(&stalled), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(stalled.requests().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_provider_failure_after_a_tool_start_reconciles_with_tools_withheld() {
    let provider = FakeProvider::new(vec![
        Script::Fail(
            vec![started("echo")],
            failure(ProviderErrorKind::ServerError, "ProviderError"),
        ),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.final_text, "done");
    assert_eq!(notes(&provider), [None, RECONCILE]);
    assert_eq!(provider.requests()[1].tool_choice, ToolChoice::None);
    assert_eq!(
        recoveries(&events)[0].action,
        Some(ModelRecoveryAction::ReconcilingTool)
    );
}

#[tokio::test(start_paused = true)]
async fn a_cut_off_call_to_a_tool_the_provider_runs_is_uncertain() {
    let provider = FakeProvider::new(vec![
        interrupted(vec![started("search")]),
        text_reply("done"),
    ]);
    let tools = vec![provider_tool("search", "Search the web."), echo_tool()];
    let mut agent = new_agent(Arc::clone(&provider), tools);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.final_text, "done");
    assert_eq!(notes(&provider), [None, RECONCILE]);
}

#[tokio::test(start_paused = true)]
async fn a_continued_turn_derives_its_tool_note_again_from_its_saved_evidence() {
    let provider = FakeProvider::new(vec![
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::ConnectivityLost, "ConnectionFailed"),
        ),
        interrupted(Vec::new()),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let recovered = RecoveredTurn {
        prompt: "fix it".to_owned(),
        messages: Vec::new(),
        files: Vec::new(),
        outputs: Vec::new(),
        strategy: RecoveryStrategy::ReconcileTool,
        fast_mode: false,
    };
    let report = agent
        .continue_turn(recovered, &mut |_| {}, &CancellationToken::new())
        .await;
    assert_eq!(report.final_text, "done");
    assert_eq!(notes(&provider), [RECONCILE, None, RECONCILE]);
    let choices: Vec<ToolChoice> = provider
        .requests()
        .iter()
        .map(|request| request.tool_choice)
        .collect();
    assert_eq!(
        choices,
        [ToolChoice::None, ToolChoice::Auto, ToolChoice::None]
    );
}
