use ofx_trace::{NetworkCall, NetworkRing};

use super::compaction::{spoken_tool_reply, unmetered, windowed};
use super::*;

fn metered(agent: &mut Agent) -> &'static NetworkRing {
    let ring: &'static NetworkRing = Box::leak(Box::new(NetworkRing::new()));
    agent.network_calls = ring;
    ring
}

fn shown(call: &NetworkCall) -> (&str, u16, &str, &str, u64) {
    (
        call.model.as_str(),
        call.status,
        call.error.as_str(),
        call.stop_reason.as_str(),
        call.subagent_id,
    )
}

#[tokio::test(start_paused = true)]
async fn each_model_request_of_a_turn_is_recorded_with_its_turn_and_step() {
    let provider = FakeProvider::new(vec![
        Script::Fail(
            Vec::new(),
            http_failure(ProviderErrorKind::Unavailable, "unavailable", 503),
        ),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::TransportInterrupted, "ReadFailed"),
        ),
        tool_reply(&[("call-1", r#"{"value":"ok"}"#)]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let ring = metered(&mut agent);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let trace = ring.snapshot();
    let calls: Vec<_> = trace.calls.iter().map(shown).collect();
    assert_eq!(
        calls,
        [
            ("test-model", 503, "", "", 0),
            ("test-model", 0, "ReadFailed", "", 0),
            ("test-model", 200, "", "tool-calls", 0),
            ("test-model", 200, "", "stop", 0),
        ]
    );
    let turn = trace.calls[0].turn_id;
    assert_ne!(turn, 0);
    assert!(trace.calls.iter().all(|call| call.turn_id == turn));
    let steps: Vec<u64> = trace.calls.iter().map(|call| call.step_id).collect();
    assert_ne!(steps[0], 0);
    assert_eq!(steps[0], steps[1]);
    assert_eq!(steps[1], steps[2]);
    assert!(steps[3] > steps[2]);
    assert_eq!(trace.calls[3].input_tokens, 10);
    assert_eq!(trace.calls[3].output_tokens, 2);
    assert_eq!(trace.calls[3].response_bytes, 4);
    assert_eq!(trace.lifetime.error_calls, 2);
    assert_eq!(trace.turns.len(), 1);
    assert_eq!(trace.turns[0].calls, 4);
}

#[tokio::test]
async fn a_subagent_run_records_its_requests_under_its_own_id() {
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let ring = metered(&mut agent);
    let trace = agent.trace_next_turn_as_subagent();
    run(&mut agent, "go").await;
    let calls = ring.snapshot().calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].turn_id, calls[0].subagent_id),
        (trace.turn_id, trace.subagent_id)
    );
    assert_eq!(ring.snapshot().turns[0].subagent_calls, 1);
}

#[tokio::test]
async fn compaction_summary_requests_are_recorded_with_the_turn_that_compacts() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply(&big_step, "call-1", r#"{"value":"notes.md"}"#),
        unmetered(text_reply(
            "Turn in progress\nIn between: Read the notes.\nT1: echoed notes.md",
        )),
        unmetered(text_reply("done")),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let ring = metered(&mut agent);
    let (report, _) = run(&mut agent, "read the notes").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let calls = ring.snapshot().calls;
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|call| call.status == 200));
    assert_eq!(calls[1].turn_id, calls[0].turn_id);
    assert_eq!(calls[1].step_id, calls[2].step_id);
}
