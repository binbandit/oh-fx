use super::*;
use crate::tool_call_metrics::{ToolCallOutcome, ToolCallRing};

fn traced(agent: &mut Agent) -> &'static ToolCallRing {
    let ring: &'static ToolCallRing = Box::leak(Box::new(ToolCallRing::new()));
    agent.tool_call_trace = ring;
    ring
}

fn recorded(ring: &ToolCallRing) -> Vec<(String, ToolCallOutcome, u64, String)> {
    ring.snapshot()
        .calls
        .into_iter()
        .map(|call| (call.name, call.outcome, call.subagent_id, call.result))
        .collect()
}

fn mixed_batch() -> Script {
    let calls = vec![
        echo_call("call-1", r#"{"value":"ok"}"#),
        echo_call("call-2", r#"{"value":"fail"}"#),
        echo_call("call-3", r#"{"value":"invalid"}"#),
        echo_call("call-4", r#"{"value":"panic"}"#),
        ToolCall::new("call-5", "missing", "{}"),
    ];
    Script::Reply(Vec::new(), completion(None, calls, FinishReason::ToolCalls))
}

#[tokio::test]
async fn each_local_tool_call_is_recorded_with_its_outcome() {
    let provider = FakeProvider::new(vec![mixed_batch(), text_reply("done")]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let ring = traced(&mut agent);
    let (report, _) = run(&mut agent, "try them").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let calls = recorded(ring);
    let outcomes: Vec<(&str, ToolCallOutcome, u64)> = calls
        .iter()
        .map(|(name, outcome, subagent, _)| (name.as_str(), *outcome, *subagent))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("echo", ToolCallOutcome::Succeeded, 0),
            ("echo", ToolCallOutcome::ToolFailed, 0),
            ("echo", ToolCallOutcome::Rejected, 0),
            ("echo", ToolCallOutcome::RuntimeFailed, 0),
            ("missing", ToolCallOutcome::Rejected, 0),
        ]
    );
    assert_eq!(calls[1].3, "echo failed");
    assert_eq!(calls[2].3, "invalid arguments");
    assert_eq!(calls[4].3, "Unsupported tool: missing");
    let snapshot = ring.snapshot();
    assert_eq!(snapshot.calls[0].args, r#"{"value":"ok"}"#);
    assert_eq!(snapshot.lifetime.total_calls, 5);
    assert!(snapshot.calls.iter().all(|call| call.started_at_ms > 0));
}

#[tokio::test]
async fn a_subagent_run_records_its_executed_calls_under_its_own_id() {
    let provider = FakeProvider::new(vec![
        mixed_batch(),
        text_reply("done"),
        tool_reply(&[("call-6", r#"{"value":"ok"}"#)]),
        text_reply("done again"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let ring = traced(&mut agent);
    let trace = agent.trace_next_turn_as_subagent();
    assert_ne!(trace.turn_id, 0);
    assert_ne!(trace.subagent_id, 0);
    run(&mut agent, "try them").await;
    let outcomes: Vec<(String, ToolCallOutcome, u64)> = recorded(ring)
        .into_iter()
        .map(|(name, outcome, subagent, _)| (name, outcome, subagent))
        .collect();
    let id = trace.subagent_id;
    assert_eq!(
        outcomes,
        [
            ("echo".to_owned(), ToolCallOutcome::Succeeded, id),
            ("echo".to_owned(), ToolCallOutcome::ToolFailed, id),
            ("echo".to_owned(), ToolCallOutcome::RuntimeFailed, id),
        ]
    );
    run(&mut agent, "again").await;
    let calls = ring.snapshot().calls;
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[3].subagent_id, 0);
}
