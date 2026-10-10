use ofx_trace::NetworkRing;

use super::*;

const MINUTE_START_MS: i64 = 1_791_000_000_000;

fn traced(calls: impl IntoIterator<Item = NetworkCall>) -> NetworkTrace {
    let ring = NetworkRing::new();
    for call in calls {
        ring.record(call);
    }
    ring.snapshot()
}

fn section(trace: &NetworkTrace) -> String {
    let mut out = String::new();
    write_section(&mut out, trace).unwrap();
    out
}

#[test]
fn the_section_reports_session_totals_turns_and_window_coverage() {
    let total = 34;
    let trace = traced((0..total).map(|index: i64| NetworkCall {
        started_at_ms: 1_000 + index * 1_000,
        duration_ms: 100,
        status: if index == 0 { 500 } else { 200 },
        turn_id: if index < 2 { 1 } else { 2 },
        model: "openai/gpt-5.6-sol".to_owned(),
        ..NetworkCall::default()
    }));
    let text = section(&trace);
    assert!(
        text.contains("\n## Network Calls\nlast=32 ok=32 errors=0 avg=100ms min=100ms max=100ms\n"),
        "{text}"
    );
    assert!(
        text.contains("session: calls=34 ok=33 errors=1 total_time=3s\n"),
        "{text}"
    );
    assert!(
        text.contains("coverage: window holds only the last 32 of 34 calls, oldest retained [1970-01-01T00:00:03.000Z]; run with OH_FX_TRACE_LOG for a complete transport record\n"),
        "{text}"
    );
    assert!(
        text.contains("turns:\n  turn 1: calls=2 errors=1 total_time=0s started [1970-01-01T00:00:01.000Z]\n  turn 2: calls=32 errors=0 total_time=3s started [1970-01-01T00:00:03.000Z]\n"),
        "{text}"
    );
}

#[test]
fn the_section_states_complete_coverage_when_nothing_was_evicted() {
    let trace = traced([NetworkCall {
        started_at_ms: 5_000,
        duration_ms: 10,
        status: 200,
        turn_id: 3,
        ..NetworkCall::default()
    }]);
    let text = section(&trace);
    assert!(text.contains("session: calls=1 ok=1 errors=0"), "{text}");
    assert!(
        text.contains("coverage: complete (window holds every recorded call)\n"),
        "{text}"
    );
    assert_eq!(
        section(&NetworkTrace::default()),
        "\n## Network Calls\n(none recorded)\n"
    );
}

#[test]
fn each_call_shows_its_source_outcome_size_tokens_and_ids() {
    let trace = traced([
        NetworkCall {
            started_at_ms: MINUTE_START_MS,
            duration_ms: 1_234,
            status: 200,
            response_bytes: 91,
            input_tokens: 1_200,
            output_tokens: 34,
            turn_id: 12,
            step_id: 34,
            model: "@openai/gpt-4o".to_owned(),
            stop_reason: "tool-calls".to_owned(),
            ..NetworkCall::default()
        },
        NetworkCall {
            started_at_ms: MINUTE_START_MS + 2_500,
            duration_ms: 17,
            status: 503,
            response_bytes: 40,
            turn_id: 12,
            step_id: 35,
            subagent_id: 4,
            model: "@openai/gpt-4o".to_owned(),
            ..NetworkCall::default()
        },
        NetworkCall {
            kind: NetworkCallKind::WebFetchTarget,
            started_at_ms: MINUTE_START_MS + 3_000,
            duration_ms: 5,
            error: "ConnectionRefused".to_owned(),
            ..NetworkCall::default()
        },
    ]);
    let text = section(&trace);
    for line in [
        "[2026-10-03T04:00:00.000Z] model=@openai/gpt-4o source=parent status=200 duration=1234ms bytes=91 tokens=1200->34 stop_reason=tool-calls turn=12 step=34\n",
        "[2026-10-03T04:00:02.500Z] model=@openai/gpt-4o source=subagent#4 status=503 duration=17ms bytes=40 turn=12 step=35\n",
        "[2026-10-03T04:00:03.000Z] model= kind=web_fetch_target source=parent err=ConnectionRefused duration=5ms bytes=0\n",
        "turns:\n  turn 12: calls=2 errors=1 total_time=1s subagent_calls=1 started [2026-10-03T04:00:00.000Z]\n",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
    assert!(
        text.contains("last=3 ok=1 errors=2 avg=418ms min=5ms max=1234ms\n"),
        "{text}"
    );
}

#[test]
fn problems_list_the_three_newest_failed_calls_first() {
    let trace = traced((0..5).map(|index: u16| NetworkCall {
        status: 500 + index,
        ..NetworkCall::default()
    }));
    let mut out = String::new();
    assert_eq!(write_problems(&mut out, &trace), Ok(3));
    let statuses: Vec<&str> = out
        .lines()
        .map(|line| {
            assert!(
                line.starts_with("- network [----------T--:--:--Z] model= source=parent status="),
                "{line}"
            );
            &line[line.find("status=").unwrap()..line.find(" duration").unwrap()]
        })
        .collect();
    assert_eq!(statuses, ["status=504", "status=503", "status=502"]);
    let mut none = String::new();
    let fine = traced([NetworkCall {
        status: 200,
        ..NetworkCall::default()
    }]);
    assert_eq!(write_problems(&mut none, &fine), Ok(0));
    assert!(none.is_empty());
}
