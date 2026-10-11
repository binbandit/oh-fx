use super::*;

fn theme() -> Theme {
    Theme::builtin(false, true, true)
}

fn totals(input_tokens: u64, output_tokens: u64, total_cost: f64) -> UsageTotals {
    UsageTotals {
        total_tokens: input_tokens + output_tokens,
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        request_count: Some(1),
        total_cost,
    }
}

fn report(models: Vec<ModelUsage>) -> UsageReport {
    let tokens = models
        .iter()
        .map(|model| model.totals.total_tokens)
        .sum::<u64>();
    let cost = models
        .iter()
        .fold(0.0, |cost, model| cost + model.totals.total_cost);
    UsageReport {
        scope: UsageScope::Days30,
        snapshot_time_ms: 100,
        window_start_ms: 0,
        coverage_started_at_ms: Some(0),
        coverage: UsageCoverage::Full,
        completeness: UsageCompleteness::Complete,
        totals: Some(totals(tokens, 0, cost)),
        models,
        session_activity: None,
    }
}

fn model(name: &str, tokens: u64, total_cost: f64) -> ModelUsage {
    ModelUsage {
        model: name.to_owned(),
        totals: totals(tokens, 0, total_cost),
    }
}

fn view(report: Option<&UsageReport>) -> UsageMenuView<'_> {
    UsageMenuView {
        scope: report.map_or(UsageScope::Days30, |report| report.scope),
        selected_model: 0,
        expanded_model: None,
        model_window_start: 0,
        report,
        refresh_failed: false,
    }
}

fn texts(view: &UsageMenuView<'_>, visible_rows: usize, width: usize) -> Vec<String> {
    usage_menu_rows(&theme(), view, visible_rows, width)
        .iter()
        .map(Row::text)
        .collect()
}

#[test]
fn the_header_lists_every_scope_when_wide_and_only_the_active_one_when_narrow() {
    let waiting = view(None);
    assert_eq!(
        texts(&waiting, 2, 80),
        [
            "Usage  [30 days]  7 days  24 hours  Session",
            "Loading usage"
        ]
    );
    assert_eq!(
        texts(&waiting, 2, 63),
        ["Usage  [30 days]", "Loading usage"]
    );
    assert_eq!(texts(&waiting, 2, 10)[0], "Usage  [30");
    let failed = UsageMenuView {
        refresh_failed: true,
        scope: UsageScope::Session,
        ..waiting
    };
    assert_eq!(
        texts(&failed, 2, 80),
        [
            "Usage  30 days  7 days  24 hours  [Session]",
            "Usage unavailable · press r to retry"
        ]
    );
    assert_eq!(usage_desired_row_count(&waiting), 2);
}

#[test]
fn the_menu_shows_status_summary_and_each_model_with_its_facts() {
    let report = report(vec![
        model("portkey/@openai/gpt-4o", 1_500, 0.25),
        model("codex/gpt-5-codex", 500, 0.0),
    ]);
    let shown = view(Some(&report));
    assert_eq!(usage_desired_row_count(&shown), 6);
    assert_eq!(
        texts(&shown, 6, 80),
        [
            "Usage  [30 days]  7 days  24 hours  Session",
            "Local oh-fx activity",
            "2K tokens · $0.25",
            "Models 2",
            "❯ portkey/@openai/gpt-4o    1.5K · 75.0% · $0.25",
            "  codex/gpt-5-codex         500 · 25.0% · $0.00",
        ]
    );
    assert_eq!(
        texts(&shown, 6, 40)[3..],
        [
            "Models",
            "❯ portkey/@openai/gpt-4o    1.5K · $0.25",
            "  codex/gpt-5-codex         500 · $0.00",
        ]
    );
    assert_eq!(
        texts(&shown, 6, 20)[4..],
        ["❯ portkey/@openai/g…", "  codex/gpt-5-codex"]
    );
}

#[test]
fn the_status_row_names_partial_unstarted_and_failed_reports() {
    let mut partial = report(Vec::new());
    partial.coverage = UsageCoverage::Partial;
    assert_eq!(
        texts(&view(Some(&partial)), 4, 80)[1],
        "Partial data · some usage may be missing"
    );
    assert_eq!(texts(&view(Some(&partial)), 4, 47)[1], "Partial data");
    let mut pending = report(Vec::new());
    pending.completeness = UsageCompleteness::Pending;
    assert_eq!(texts(&view(Some(&pending)), 4, 47)[1], "Partial data");
    let mut unstarted = report(Vec::new());
    unstarted.coverage = UsageCoverage::NotStarted;
    unstarted.totals = None;
    assert_eq!(
        texts(&view(Some(&unstarted)), 3, 80),
        [
            "Usage  [30 days]  7 days  24 hours  Session",
            "Tracking has not started",
            ""
        ]
    );
    let refreshed = UsageMenuView {
        refresh_failed: true,
        ..view(Some(&partial))
    };
    assert_eq!(
        texts(&refreshed, 5, 80)[1..],
        [
            "Refresh failed · showing previous data",
            "0 tokens · $0.00",
            "Models 0",
            "Refresh failed · showing previous data",
        ]
    );
}

#[test]
fn an_expanded_model_shows_its_detail_and_the_window_follows_the_selection() {
    let models = (0..6)
        .map(|index| model(&format!("provider/m{index}"), 60 - index * 10, 0.0))
        .collect();
    let mut report = report(models);
    report.models[3].totals = UsageTotals {
        reasoning_tokens: Some(4),
        request_count: Some(1_234),
        cache_read_tokens: 2,
        ..totals(30, 10, 0.5)
    };
    let expanded = UsageMenuView {
        selected_model: 3,
        expanded_model: Some(3),
        ..view(Some(&report))
    };
    assert_eq!(usage_desired_row_count(&expanded), 11);
    let rows = texts(&expanded, 7, 120);
    assert_eq!(rows[4], "  provider/m2    40 · 19.0% · $0.00");
    assert_eq!(rows[5], "❯ provider/m3    40 · 19.0% · $0.50");
    assert_eq!(
        rows[6],
        "Input 30 · Output 10 · Cache 2/0 · Reasoning 4 · Requests 1,234 · $0.50"
    );
    assert_eq!(usage_visible_model_items(&expanded, 7), 2);
    assert_eq!(usage_visible_model_items(&expanded, 11), 6);
    assert_eq!(usage_visible_model_items(&view(Some(&report)), 1), 1);
}

#[test]
fn a_single_row_shows_the_selected_model_or_the_status() {
    let single = report(vec![model("provider/model", 7, 0.25)]);
    assert_eq!(
        texts(&view(Some(&single)), 1, 80),
        ["❯ provider/model    7 · 100.0% · $0.25"]
    );
    let empty = report(Vec::new());
    assert_eq!(texts(&view(Some(&empty)), 1, 80), ["Local oh-fx activity"]);
    assert_eq!(texts(&view(None), 1, 80), ["Loading usage"]);
}

#[test]
fn every_row_fits_its_width() {
    let report = report(vec![
        model("portkey/@openai/gpt-4o-with-a-long-name", 1_234_567, 12.5),
        model("codex/gpt-5-codex", 89, 0.0),
    ]);
    let expanded = UsageMenuView {
        expanded_model: Some(0),
        ..view(Some(&report))
    };
    for width in 1..=90 {
        for visible_rows in 1..=8 {
            for row in usage_menu_rows(&theme(), &expanded, visible_rows, width) {
                assert!(
                    row.width() <= width,
                    "{width}x{visible_rows}: {:?}",
                    row.text()
                );
            }
        }
        assert!(usage_menu_hint_row(&theme(), width).width() <= width);
    }
    assert_eq!(
        usage_menu_hint_row(&theme(), 60).text(),
        "tab scope  ↑↓ model  enter expand  r refresh  esc"
    );
}

#[test]
fn numbers_read_compactly_grouped_and_as_money_and_shares() {
    let compact: Vec<String> = [0, 999, 1_000, 1_500, 2_000_000, 2_340_000, 1_260_000_000]
        .into_iter()
        .map(compact_unsigned)
        .collect();
    assert_eq!(compact, ["0", "999", "1K", "1.5K", "2M", "2.3M", "1.3B"]);
    let grouped: Vec<String> = [0, 999, 1_000, 1_234_567]
        .into_iter()
        .map(grouped_unsigned)
        .collect();
    assert_eq!(grouped, ["0", "999", "1,000", "1,234,567"]);
    assert_eq!(money(0.25), "$0.25");
    assert_eq!(usage_share(1, 3), "33.3%");
    assert_eq!(usage_share(0, 0), "0.0%");
    assert_eq!(
        float((1 << 40) + 3).to_bits(),
        1_099_511_627_779.0_f64.to_bits()
    );
}
