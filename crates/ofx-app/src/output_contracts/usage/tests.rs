use ofx_contract::{ModelUsage, UsageScope};

use super::*;

fn totals(total_tokens: u64, total_cost: f64) -> UsageTotals {
    UsageTotals {
        total_tokens,
        input_tokens: total_tokens - 2,
        output_tokens: 2,
        cache_read_tokens: 3,
        cache_write_tokens: 1,
        reasoning_tokens: None,
        request_count: Some(1),
        total_cost,
    }
}

fn report(coverage: UsageCoverage, completeness: UsageCompleteness) -> UsageReport {
    UsageReport {
        scope: UsageScope::Days7,
        snapshot_time_ms: 200,
        window_start_ms: 100,
        coverage_started_at_ms: (coverage != UsageCoverage::NotStarted).then_some(150),
        coverage,
        completeness,
        totals: (coverage != UsageCoverage::NotStarted).then(|| totals(12, 0.25)),
        models: Vec::new(),
    }
}

#[test]
fn text_and_json_render_the_same_optional_and_ordered_facts() {
    let report = UsageReport {
        models: vec![ModelUsage {
            model: "provider/model".to_owned(),
            totals: totals(12, 0.25),
        }],
        ..report(UsageCoverage::Partial, UsageCompleteness::Complete)
    };
    let snapshot = UsageSnapshot { report: &report };
    assert_eq!(
        snapshot.render(OutputFormat::Text),
        "Usage (7 days)\nTracking since Jan 1, 1970 (partial window).\nTotal tokens  12\nInput         10\nOutput        2\nCache         3 read · 1 write\nRequests      1\nSpend         $0.2500\n\nBy model\n- provider/model  12 tokens  $0.2500\n"
    );
    assert_eq!(
        snapshot.render(OutputFormat::Json),
        "{\"kind\":\"usage\",\"schema_version\":1,\"period\":\"7d\",\"snapshot_time_ms\":200,\"window_start_ms\":100,\"coverage\":{\"status\":\"partial\",\"started_at_ms\":150,\"full_window\":false},\"completeness\":\"complete\",\"totals\":{\"total_tokens\":12,\"input_tokens\":10,\"output_tokens\":2,\"cache_read_tokens\":3,\"cache_write_tokens\":1,\"reasoning_tokens\":null,\"request_count\":1,\"spend\":0.25},\"models\":[{\"model\":\"provider/model\",\"totals\":{\"total_tokens\":12,\"input_tokens\":10,\"output_tokens\":2,\"cache_read_tokens\":3,\"cache_write_tokens\":1,\"reasoning_tokens\":null,\"request_count\":1,\"spend\":0.25}}]}\n"
    );
}

#[test]
fn reports_without_tracking_print_only_their_heading_and_state() {
    let report = report(UsageCoverage::NotStarted, UsageCompleteness::Complete);
    let snapshot = UsageSnapshot { report: &report };
    assert_eq!(
        snapshot.render(OutputFormat::Text),
        "Usage (7 days)\nTracking has not started.\n"
    );
    assert_eq!(
        snapshot.render(OutputFormat::Json),
        "{\"kind\":\"usage\",\"schema_version\":1,\"period\":\"7d\",\"snapshot_time_ms\":200,\"window_start_ms\":100,\"coverage\":{\"status\":\"not_started\",\"started_at_ms\":null,\"full_window\":false},\"completeness\":\"complete\",\"totals\":null,\"models\":[]}\n"
    );
}

#[test]
fn completeness_lines_follow_coverage_and_precede_totals() {
    for (completeness, line) in [
        (
            UsageCompleteness::Pending,
            "Known totals exclude pending Gateway reconciliation.\n",
        ),
        (
            UsageCompleteness::Incomplete,
            "Known totals may be incomplete.\n",
        ),
        (
            UsageCompleteness::Legacy,
            "This session predates complete usage tracking.\n",
        ),
    ] {
        let full = report(UsageCoverage::Full, completeness);
        let text = UsageSnapshot { report: &full }.render(OutputFormat::Text);
        assert!(
            text.starts_with(&format!("Usage (7 days)\n{line}Total tokens  12\n")),
            "{text}"
        );
        let unstarted = report(UsageCoverage::NotStarted, completeness);
        assert_eq!(
            UsageSnapshot { report: &unstarted }.render(OutputFormat::Text),
            format!("Usage (7 days)\nTracking has not started.\n{line}")
        );
    }
}

#[test]
fn optional_counters_print_only_when_known_and_spend_rounds_half_up() {
    let mut report = report(UsageCoverage::Full, UsageCompleteness::Complete);
    report.scope = UsageScope::Session;
    report.totals = Some(UsageTotals {
        reasoning_tokens: Some(4),
        request_count: None,
        total_cost: 0.031_25,
        ..totals(20, 0.0)
    });
    report.models = vec![
        ModelUsage {
            model: "a\u{1b}[31m".to_owned(),
            totals: totals(15, 1.000_05),
        },
        ModelUsage {
            model: "b".to_owned(),
            totals: totals(5, 1e-7),
        },
    ];
    let snapshot = UsageSnapshot { report: &report };
    assert_eq!(
        snapshot.render(OutputFormat::Text),
        "Usage (Session)\nTotal tokens  20\nInput         18\nOutput        2\nCache         3 read · 1 write\nReasoning     4\nSpend         $0.0313\n\nBy model\n- a\\x1b[31m  15 tokens  $1.0001\n- b  5 tokens  $0.0000\n"
    );
    let json = snapshot.render(OutputFormat::Json);
    assert!(json.starts_with("{\"kind\":\"usage\",\"schema_version\":1,\"period\":\"session\","));
    assert!(json.contains("\"reasoning_tokens\":4,\"request_count\":null,\"spend\":0.03125}"));
    assert!(json.contains("{\"model\":\"a\\u001b[31m\",\"totals\""));
    assert!(json.contains("\"spend\":0.0000001}"));
    assert!(json.contains("\"full_window\":true}"));
}
