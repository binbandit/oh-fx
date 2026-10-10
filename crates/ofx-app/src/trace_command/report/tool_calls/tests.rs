use super::*;

const STARTED_MS: i64 = 1_780_000_000_123;
const STAMP: &str = "[2026-05-28T20:26:40.123Z]";

fn call(name: &str, outcome: ToolCallOutcome, args: &str, result: &str) -> ToolCallMetric {
    ToolCallMetric {
        started_at_ms: STARTED_MS,
        duration_ms: 12,
        outcome,
        subagent_id: 0,
        name: name.to_owned(),
        args: args.to_owned(),
        args_total_bytes: u32::try_from(args.len()).unwrap(),
        result: result.to_owned(),
        result_total_bytes: u32::try_from(result.len()).unwrap(),
    }
}

fn traced(calls: Vec<ToolCallMetric>) -> ToolCallTrace {
    let mut trace = ToolCallTrace::default();
    for call in &calls {
        trace.lifetime.total_calls += 1;
        trace.lifetime.total_duration_ms += u64::from(call.duration_ms);
        trace.lifetime.outcome_counts[ToolCallOutcome::ALL
            .iter()
            .position(|outcome| *outcome == call.outcome)
            .unwrap()] += 1;
    }
    trace.calls = calls;
    trace
}

fn section(trace: &ToolCallTrace) -> String {
    searched(trace, &[])
}

fn searched(trace: &ToolCallTrace, searches: &[Option<ToolResultStatus>]) -> String {
    let mut out = String::new();
    write_section(&mut out, trace, searches).unwrap();
    out
}

#[test]
fn trace_tool_calls_preserve_outcomes_and_mask_obvious_secrets() {
    let trace = traced(vec![
        call(
            "read_file",
            ToolCallOutcome::Succeeded,
            "{\"path\":\"README.md\"}",
            "# fx",
        ),
        call(
            "edit_file",
            ToolCallOutcome::Rejected,
            "{\"path\":\"a.zig\"}",
            "{\"error\":\"tool_permission_denied\"}",
        ),
        call(
            "shell",
            ToolCallOutcome::CommandFailed,
            "{\"command\":\"zig build\"}",
            "exit 1",
        ),
        call(
            "read_file",
            ToolCallOutcome::ToolFailed,
            "{\"path\":\"missing\"}",
            "not found",
        ),
        ToolCallMetric {
            subagent_id: 3,
            ..call(
                "run_command",
                ToolCallOutcome::RuntimeFailed,
                "{\"command\":\"AI_GATEWAY_API_KEY=abcdefghijklmnop zig build\"}",
                "failed with PASSWORD=abcdefghijklmnop",
            )
        },
    ]);
    let report = section(&trace);
    assert!(report.contains(
        "last=5 succeeded=1 rejected=1 command_failed=1 tool_failed=1 runtime_failed=1 total=60ms\n"
    ));
    for line in [
        "name=edit_file outcome=rejected",
        "name=shell outcome=command_failed",
        "name=read_file outcome=tool_failed",
        "name=run_command outcome=runtime_failed",
    ] {
        assert!(report.contains(line), "{line}\n{report}");
    }
    let failed = report.find("name=run_command").unwrap();
    let succeeded = report.find("name=read_file outcome=succeeded").unwrap();
    assert!(failed < succeeded);
    assert!(report.contains(" source=subagent#3\n"));
    assert!(!report.contains("abcdefghijklmnop"));
    assert!(report.contains("AI_GATEWAY_API_KEY=[redacted]"));
    assert!(report.contains("PASSWORD=[redacted]"));
    let mut problems = String::new();
    assert_eq!(write_problems(&mut problems, &trace).unwrap(), 4);
    assert!(problems.starts_with(&format!(
        "- tool {STAMP} name=run_command outcome=runtime_failed duration=12ms source=subagent#3\n"
    )));
}

#[test]
fn trace_successful_tool_calls_use_compact_result_previews() {
    let trace = traced(vec![call(
        "read_file",
        ToolCallOutcome::Succeeded,
        "{\"path\":\"README.md\"}",
        "\n:\n# fx\nlong body line\n",
    )]);
    let report = section(&trace);
    assert!(report.contains("recent successes (compact):\n"));
    assert!(
        report.contains("  result_preview: # fx (23 bytes total)\n"),
        "{report}"
    );
    assert!(!report.contains("long body line"));
    assert!(!report.contains("non-successes first:"));
}

#[test]
fn trace_omits_web_fetch_url_and_result_bodies() {
    let trace = traced(vec![call("web_fetch", ToolCallOutcome::Succeeded, "", "")]);
    let report = section(&trace);
    assert!(report.contains(&format!(
        "{STAMP} name=web_fetch outcome=succeeded duration=12ms source=parent\n"
    )));
    assert!(!report.contains("args:"));
    assert!(!report.contains("result_preview:"));
}

#[test]
fn trace_tool_section_reports_session_totals_and_window_coverage() {
    let mut trace = traced(vec![call("shell", ToolCallOutcome::Succeeded, "{}", "ok")]);
    let report = section(&trace);
    assert!(report.contains("coverage: complete (window holds every recorded tool call)\n"));
    trace.lifetime.total_calls = 66;
    trace.lifetime.outcome_counts[0] = 65;
    trace.lifetime.outcome_counts[1] = 1;
    let report = section(&trace);
    assert!(report.contains(
        "session: calls=66 succeeded=65 rejected=1 command_failed=0 tool_failed=0 runtime_failed=0 total_time=0s\n"
    ));
    assert!(report.contains(
        "coverage: window holds only the last 1 of 66 tool calls; full results persist in the session directory\n"
    ));
    assert_eq!(
        section(&ToolCallTrace::default()),
        "\n## Tool Calls\n(none recorded)\n"
    );
}

#[test]
fn long_fields_show_every_line_and_how_much_was_cut() {
    let mut cut = call(
        "shell",
        ToolCallOutcome::ToolFailed,
        "{\"command\":\"make\"}",
        "first line  \nsecond line\r",
    );
    cut.args_total_bytes += 40;
    cut.result_total_bytes += 7;
    let report = section(&traced(vec![cut]));
    assert!(report.contains("  args: {\"command\":\"make\"} ... (40 more bytes)\n"));
    assert!(
        report.contains("  result:\n    first line\n    second line\n    ... (7 more bytes)\n"),
        "{report}"
    );
}

#[test]
fn a_provider_search_alias_is_shown_as_web_search() {
    let trace = traced(vec![call("exa_search", ToolCallOutcome::Rejected, "", "")]);
    let report = section(&trace);
    assert!(report.contains(" name=web_search outcome=rejected "));
    assert!(!report.contains("exa_search"));
}

#[test]
fn web_searches_show_only_their_status_under_the_neutral_name() {
    let searches = [
        Some(ToolResultStatus::Success),
        Some(ToolResultStatus::Failure),
        None,
    ];
    assert_eq!(
        searched(&ToolCallTrace::default(), &searches),
        "\n## Tool Calls\n### Local\n(none locally executed)\n### Web Search\nlast=3\nname=web_search status=ok\nname=web_search status=err\nname=web_search status=pending\n"
    );
}

#[test]
fn local_calls_without_web_searches_say_none_are_retained() {
    let trace = traced(vec![call(
        "read_file",
        ToolCallOutcome::Succeeded,
        "{}",
        "text",
    )]);
    assert!(
        section(&trace).ends_with("### Web Search\n(none retained)\n"),
        "{}",
        section(&trace)
    );
}
