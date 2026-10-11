use ofx_contract::{FileChangeStats, UsageCoverage, UsageScope};

use super::*;
use crate::json_fields::parse_json;
use crate::session_usage::Usage;

const NOW: i64 = 1_775_045_467_000;

#[test]
fn a_session_report_keeps_usage_first_totals_and_session_activity() {
    let mut usage = Usage::fresh();
    usage.record_committed_lines(FileChangeStats {
        additions: 3,
        deletions: 1,
    });
    let report = usage.snapshot(NOW).report(NOW).unwrap();
    assert_eq!(report.scope, UsageScope::Session);
    assert_eq!(report.totals.unwrap().total_tokens, 0);
    let activity = report.session_activity.unwrap();
    assert_eq!((activity.lines_added, activity.lines_removed), (3, 1));
    assert!(activity.code_complete);
}

#[test]
fn a_session_report_starts_its_window_one_wall_duration_before_the_snapshot() {
    let text = concat!(
        "{\"schema_version\":3,\"billing\":\"incomplete\",\"api_duration_complete\":true,",
        "\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":3,",
        "\"settled_through_sequence\":2,\"api_duration_ms\":1500,\"wall_duration_ms\":60000,",
        "\"total_cost\":0.0125,\"input_tokens\":140,\"output_tokens\":40,\"cache_read_tokens\":20,",
        "\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"request_count\":2,",
        "\"billable_web_search_calls\":0,\"lines_added\":7,\"lines_removed\":2,\"models\":[",
        "{\"model\":\"codex/gpt-5-codex\",\"first_sequence\":1,\"total_cost\":0,",
        "\"input_tokens\":20,\"output_tokens\":10,\"cache_read_tokens\":0,",
        "\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"request_count\":1,",
        "\"billable_web_search_calls\":0},",
        "{\"model\":\"openai/gpt-5\",\"first_sequence\":2,\"total_cost\":0.0125,",
        "\"input_tokens\":120,\"output_tokens\":30,\"cache_read_tokens\":20,",
        "\"cache_write_tokens\":0,\"reasoning_tokens\":5,\"request_count\":1,",
        "\"billable_web_search_calls\":0}],\"pending\":[],\"publication_backlog\":[],",
        "\"incidents\":[]}"
    );
    let snapshot = UsageSnapshot::parse_rich(&parse_json(text.as_bytes()).unwrap()).unwrap();
    let report = snapshot.report(NOW).unwrap();
    assert_eq!(report.window_start_ms, NOW - 60_000);
    assert_eq!(report.coverage_started_at_ms, Some(NOW - 60_000));
    assert_eq!(report.coverage, UsageCoverage::Full);
    assert_eq!(report.completeness, UsageCompleteness::Incomplete);
    let totals = report.totals.unwrap();
    assert_eq!(
        (
            totals.total_tokens,
            totals.request_count,
            totals.reasoning_tokens
        ),
        (180, Some(2), None)
    );
    let models: Vec<(&str, u64)> = report
        .models
        .iter()
        .map(|model| (model.model.as_str(), model.totals.total_tokens))
        .collect();
    assert_eq!(models, [("openai/gpt-5", 150), ("codex/gpt-5-codex", 30)]);
    assert_eq!(report.models[0].totals.reasoning_tokens, Some(5));
    let early = snapshot.report(30_000).unwrap();
    assert_eq!(early.window_start_ms, 0);
}
