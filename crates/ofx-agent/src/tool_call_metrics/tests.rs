use super::*;

fn metric(duration_ms: u32, outcome: ToolCallOutcome) -> ToolCallMetric {
    ToolCallMetric {
        started_at_ms: 0,
        duration_ms,
        outcome,
        subagent_id: 0,
        name: "read_file".to_owned(),
        args: String::new(),
        args_total_bytes: 0,
        result: String::new(),
        result_total_bytes: 0,
    }
}

fn recorded<'a>(name: &'a str, arguments: &'a str, output: &'a str) -> ToolCallRecord<'a> {
    ToolCallRecord {
        name,
        arguments,
        output,
        outcome: ToolCallOutcome::Succeeded,
        started_at_ms: ofx_trace::timestamp_ms(),
        finished_at_ms: ofx_trace::timestamp_ms(),
        subagent_id: 0,
    }
}

#[test]
fn the_tool_call_ring_keeps_the_last_records_in_chronological_order() {
    let ring = ToolCallRing::new();
    for index in 0..RING_CAPACITY + 3 {
        ring.push(metric(
            u32::try_from(index).unwrap(),
            ToolCallOutcome::Succeeded,
        ));
    }
    let trace = ring.snapshot();
    assert_eq!(trace.calls.len(), RING_CAPACITY);
    assert_eq!(trace.calls[0].duration_ms, 3);
    assert_eq!(
        trace.calls[RING_CAPACITY - 1].duration_ms,
        u32::try_from(RING_CAPACITY + 2).unwrap()
    );
}

#[test]
fn args_and_result_are_truncated_and_their_total_length_tracked() {
    let ring = ToolCallRing::new();
    let long = "a".repeat(2200);
    ring.record(&recorded("read_file", &long, &long));
    let call = &ring.snapshot().calls[0];
    assert_eq!(call.args.len(), MAX_ARGS_BYTES);
    assert_eq!(call.args_total_bytes, 2200);
    assert_eq!(call.result.len(), MAX_RESULT_BYTES);
    assert_eq!(call.result_total_bytes, 2200);
}

#[test]
fn lifetime_stats_count_every_outcome_and_survive_ring_eviction() {
    let ring = ToolCallRing::new();
    for index in 0..RING_CAPACITY + 3 {
        let outcome = if index % 4 == 0 {
            ToolCallOutcome::Rejected
        } else {
            ToolCallOutcome::Succeeded
        };
        ring.push(metric(5, outcome));
    }
    let trace = ring.snapshot();
    let total = u64::try_from(RING_CAPACITY + 3).unwrap();
    assert_eq!(trace.lifetime.total_calls, total);
    assert_eq!(
        trace.lifetime.count_for(ToolCallOutcome::Succeeded)
            + trace.lifetime.count_for(ToolCallOutcome::Rejected),
        total
    );
    assert!(trace.lifetime.count_for(ToolCallOutcome::Rejected) > 0);
    assert_eq!(trace.lifetime.total_duration_ms, total * 5);
    assert!(trace.lifetime.total_calls > u64::try_from(trace.calls.len()).unwrap());
    ring.reset();
    let cleared = ring.snapshot();
    assert_eq!(cleared.lifetime.total_calls, 0);
    assert!(cleared.calls.is_empty());
}

#[test]
fn tool_call_outcome_labels_remain_exact() {
    assert_eq!(
        ToolCallOutcome::ALL.map(ToolCallOutcome::name),
        [
            "succeeded",
            "rejected",
            "command_failed",
            "tool_failed",
            "runtime_failed"
        ]
    );
}

#[test]
fn a_web_fetch_call_keeps_no_url_or_fetched_content() {
    let ring = ToolCallRing::new();
    ring.record(&recorded(
        "web_fetch",
        "{\"url\":\"https://example.com/?token=secret\"}",
        "<content>\nsecret page\n</content>",
    ));
    let call = &ring.snapshot().calls[0];
    assert_eq!(call.name, "web_fetch");
    assert_eq!(call.outcome, ToolCallOutcome::Succeeded);
    assert!(call.args.is_empty() && call.result.is_empty());
    assert_eq!((call.args_total_bytes, call.result_total_bytes), (0, 0));
}

#[test]
fn a_result_keeps_the_body_of_its_envelope_and_names_are_cut() {
    let ring = ToolCallRing::new();
    let name = format!("{}é", "n".repeat(MAX_NAME_BYTES - 1));
    ring.record(&ToolCallRecord {
        subagent_id: 3,
        outcome: ToolCallOutcome::ToolFailed,
        ..recorded(
            &name,
            "{\"path\":\"README.md\"}",
            "<path>README.md</path>\n<content>\n1\t# fx\n</content>",
        )
    });
    let call = &ring.snapshot().calls[0];
    assert_eq!(call.name.len(), MAX_NAME_BYTES - 1);
    assert_eq!(call.args, "{\"path\":\"README.md\"}");
    assert_eq!(call.result, "1\t# fx");
    assert_eq!(call.result_total_bytes, 6);
    assert_eq!(call.subagent_id, 3);
    assert_eq!(call.outcome, ToolCallOutcome::ToolFailed);
}

#[test]
fn a_duration_runs_from_the_start_to_the_finish_and_is_never_negative() {
    let ring = ToolCallRing::new();
    ring.record(&ToolCallRecord {
        started_at_ms: ofx_trace::timestamp_ms() + 60_000,
        ..recorded("shell", "{}", "")
    });
    ring.record(&ToolCallRecord {
        started_at_ms: ofx_trace::timestamp_ms() - 1_500,
        ..recorded("shell", "{}", "")
    });
    ring.record(&ToolCallRecord {
        started_at_ms: 1_000,
        finished_at_ms: 3_250,
        ..recorded("shell", "{}", "")
    });
    let calls = ring.snapshot().calls;
    assert_eq!(calls[0].duration_ms, 0);
    assert!(calls[1].duration_ms >= 1_500);
    assert_eq!(calls[2].duration_ms, 2_250);
}
