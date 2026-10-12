use ofx_contract::{ModelUsage, SessionActivity};
use ofx_text::visible_width;

use super::numbers::{float, format_duration, grouped_unsigned, request_label, usage_share};
use super::*;

fn theme() -> Theme {
    Theme::builtin(false, true, true)
}

fn totals(tokens: u64, total_cost: f64) -> UsageTotals {
    UsageTotals {
        total_tokens: tokens,
        input_tokens: tokens,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        request_count: Some(1),
        total_cost,
    }
}

fn model(name: &str, tokens: u64, total_cost: f64) -> ModelUsage {
    ModelUsage {
        model: name.to_owned(),
        totals: totals(tokens, total_cost),
    }
}

fn report(
    scope: UsageScope,
    report_totals: Option<UsageTotals>,
    models: Vec<ModelUsage>,
) -> UsageReport {
    UsageReport {
        scope,
        snapshot_time_ms: 100,
        window_start_ms: 0,
        coverage_started_at_ms: Some(0),
        coverage: UsageCoverage::Full,
        completeness: UsageCompleteness::Complete,
        totals: report_totals,
        models,
        session_activity: None,
    }
}

fn pair() -> UsageReport {
    report(
        UsageScope::Days30,
        Some(totals(2_000, 0.25)),
        vec![
            model("portkey/@openai/gpt-4o", 1_500, 0.25),
            model("codex/gpt-5-codex", 500, 0.0),
        ],
    )
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

fn row(view: &UsageMenuView<'_>, row_index: usize, visible_rows: usize, width: usize) -> String {
    texts(view, visible_rows, width).swap_remove(row_index)
}

const ACTIVITY: SessionActivity = SessionActivity {
    api_duration_complete: true,
    wall_duration_complete: false,
    code_complete: true,
    api_duration_ms: 1_200,
    wall_duration_ms: 3_400,
    lines_added: 5,
    lines_removed: 2,
};

#[test]
fn the_header_lists_every_scope_when_wide_and_only_the_active_one_when_narrow() {
    let waiting = view(None);
    assert_eq!(usage_desired_row_count(&waiting, 80), 2);
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
}

#[test]
fn a_medium_menu_shows_the_overview_then_each_model_with_its_facts() {
    let pair = pair();
    let shown = view(Some(&pair));
    assert_eq!(usage_desired_row_count(&shown, 80), 10);
    assert_eq!(
        texts(&shown, 10, 80),
        [
            "Usage  [30 days]  7 days  24 hours  Session",
            "Local oh-fx activity",
            "",
            "2K tokens · $0.25 spent · 1 request",
            "2K input · 0 output · Reasoning unavailable",
            "0 cache read · 0 cache write",
            "",
            "Models 2",
            "❯ portkey/@openai/gpt-4o    1.5K · 75.0% · $0.25",
            "  codex/gpt-5-codex         500 · 25.0% · $0.00",
        ]
    );
}

#[test]
fn a_wide_menu_aligns_the_overview_and_the_model_columns() {
    let pair = pair();
    let shown = view(Some(&pair));
    assert_eq!(usage_desired_row_count(&shown, 120), 11);
    assert_eq!(
        texts(&shown, 11, 120)[3..],
        [
            "  2K tokens       $0.25 spent      1 request",
            "  2K input        0 output         Reasoning unavailable",
            "  0 cache read    0 cache write",
            "",
            "Models 2",
            "  Model                     Tokens    Share    Spend",
            "❯ portkey/@openai/gpt-4o    1.5K      75.0%    $0.25",
            "  codex/gpt-5-codex         500       25.0%    $0.00",
        ]
    );
}

#[test]
fn a_narrow_menu_keeps_two_overview_rows_and_compact_model_facts() {
    let pair = pair();
    let shown = view(Some(&pair));
    assert_eq!(usage_desired_row_count(&shown, 40), 7);
    assert_eq!(
        texts(&shown, 7, 40),
        [
            "Usage  [30 days]",
            "Local oh-fx activity",
            "2K tokens · $0.25",
            "1 request",
            "Models",
            "❯ portkey/@openai/gpt-4o    1.5K · $0.25",
            "  codex/gpt-5-codex         500 · $0.00",
        ]
    );
}

#[test]
fn a_short_menu_falls_back_to_the_summary_layout() {
    let pair = pair();
    let shown = view(Some(&pair));
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
        texts(&shown, 3, 80)[1..],
        [
            "Local oh-fx activity",
            "❯ portkey/@openai/gpt-4o    1.5K · 75.0% · $0.25"
        ]
    );
    assert_eq!(usage_visible_model_items(&shown, 6, 80), 2);
    assert_eq!(usage_visible_model_items(&shown, 3, 80), 1);
}

#[test]
fn the_menu_renders_token_first_totals_and_selectable_model_rows() {
    let usage = report(
        UsageScope::Days30,
        Some(UsageTotals {
            total_tokens: 120,
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 20,
            cache_write_tokens: 10,
            reasoning_tokens: Some(5),
            request_count: Some(2),
            total_cost: 0.0123,
        }),
        vec![
            model("z.ai/glm-5.2", 104, 0.0104),
            model("openai/gpt-5.6", 19, 0.0019),
        ],
    );
    let shown = view(Some(&usage));
    let total = row(&shown, 3, 14, 120);
    for fact in ["120 tokens", "$0.01 spent", "2 requests"] {
        assert!(total.contains(fact), "{total}");
    }
    assert_eq!(usage_desired_row_count(&shown, 120), 11);
    let first = row(&shown, 9, 11, 120);
    for fact in ["z.ai/glm-5.2", "104", "86.7%"] {
        assert!(first.contains(fact), "{first}");
    }
    let narrow = row(&shown, 2, 8, 40);
    assert!(narrow.contains("120 tokens"), "{narrow}");
    assert!(narrow.contains("$0.01"), "{narrow}");
}

#[test]
fn the_menu_adapts_its_density_without_losing_model_columns() {
    let mut usage = report(
        UsageScope::Days7,
        Some(UsageTotals {
            total_tokens: 61_114_267,
            input_tokens: 60_415_084,
            output_tokens: 699_183,
            cache_read_tokens: 42_920_431,
            cache_write_tokens: 14_728_752,
            reasoning_tokens: Some(253_917),
            request_count: Some(1_464),
            total_cost: 117.2854,
        }),
        vec![
            ModelUsage {
                model: "openai/gpt-5.6-sol".to_owned(),
                totals: UsageTotals {
                    total_tokens: 43_626_823,
                    input_tokens: 43_000_000,
                    output_tokens: 626_823,
                    cache_read_tokens: 30_000_000,
                    cache_write_tokens: 5_000_000,
                    reasoning_tokens: Some(200_000),
                    request_count: Some(1_000),
                    total_cost: 100.7886,
                },
            },
            model("provider/a-much-longer-model-name", 17_487_444, 16.4968),
        ],
    );
    usage.coverage = UsageCoverage::Partial;
    usage.completeness = UsageCompleteness::Incomplete;
    let shown = view(Some(&usage));

    let small_header = row(&shown, 0, 7, 44);
    assert!(small_header.contains("[7 days]"), "{small_header}");
    assert!(!small_header.contains("30 days"), "{small_header}");
    let small_model = row(&shown, 5, 7, 44);
    assert!(small_model.contains("43.6M"), "{small_model}");
    assert!(small_model.contains("$100.79"), "{small_model}");
    assert!(!small_model.contains('%'), "{small_model}");
    let medium_model = row(&shown, 8, 10, 90);
    assert!(
        medium_model.contains("43.6M · 71.4% · $100.79"),
        "{medium_model}"
    );
    let wide_header = row(&shown, 8, 11, 160);
    for column in ["Model", "Tokens", "Share", "Spend"] {
        assert!(wide_header.contains(column), "{wide_header}");
    }
    for width in [32, 44, 90, 160] {
        let rows = usage_desired_row_count(&shown, width);
        for drawn in usage_menu_rows(&theme(), &shown, rows, width) {
            assert!(drawn.width() <= width, "{width}: {:?}", drawn.text());
        }
    }
}

#[test]
fn the_menu_shows_at_most_twenty_models_and_aligns_their_facts() {
    let models: Vec<ModelUsage> = (0..25)
        .map(|index| {
            let name = if index == 24 {
                "provider/a-much-longer-selected-model"
            } else {
                "provider/model"
            };
            model(name, 10, 0.001)
        })
        .collect();
    let usage = report(UsageScope::Days30, Some(totals(250, 0.025)), models);
    let shown = UsageMenuView {
        selected_model: 24,
        model_window_start: 24,
        ..view(Some(&usage))
    };
    assert_eq!(usage_desired_row_count(&shown, 80), 28);
    let header = row(&shown, 0, 28, 80);
    assert!(
        header.contains("[30 days]") && header.contains("Session"),
        "{header}"
    );
    assert!(row(&shown, 0, 1, 100).contains("selected-model"));

    let pair = report(
        UsageScope::Days30,
        Some(totals(50, 0.005)),
        vec![
            model("provider/short", 20, 0.002),
            model("provider/a-much-longer-model", 30, 0.003),
        ],
    );
    let pair_view = view(Some(&pair));
    let expected_column = 2 + visible_width("provider/a-much-longer-model") + 4;
    for (drawn, label) in [
        (row(&pair_view, 8, 10, 80), "medium"),
        (row(&pair_view, 9, 11, 180), "wide"),
    ] {
        let facts = drawn
            .find("20")
            .unwrap_or_else(|| panic!("{label}: {drawn}"));
        assert_eq!(
            visible_width(&drawn[..facts]),
            expected_column,
            "{label}: {drawn}"
        );
    }
}

#[test]
fn an_expanded_last_session_model_shows_its_detail() {
    let models = (0..13)
        .map(|_| model("provider/model", 10, 0.001))
        .collect();
    let mut usage = report(UsageScope::Session, Some(totals(130, 0.013)), models);
    usage.session_activity = Some(SessionActivity {
        api_duration_complete: true,
        wall_duration_complete: true,
        code_complete: true,
        api_duration_ms: 1,
        wall_duration_ms: 2,
        lines_added: 3,
        lines_removed: 4,
    });
    let shown = UsageMenuView {
        selected_model: 12,
        expanded_model: Some(12),
        model_window_start: 1,
        ..view(Some(&usage))
    };
    assert!(row(&shown, 24, 26, 100).contains("❯ provider/model"));
    assert!(row(&shown, 25, 26, 100).contains("Input 10 · Output 0"));
}

#[test]
fn an_expanded_model_stays_visible_at_a_constrained_height() {
    let models = (0..13)
        .map(|_| model("provider/model", 10, 0.001))
        .collect();
    let usage = report(UsageScope::Days30, Some(totals(130, 0.013)), models);
    let shown = UsageMenuView {
        selected_model: 12,
        expanded_model: Some(12),
        model_window_start: 12,
        ..view(Some(&usage))
    };
    assert!(row(&shown, 11, 12, 72).contains("Input 10 · Output 0"));
}

#[test]
fn a_pending_session_without_totals_shows_its_status_once() {
    let mut usage = report(UsageScope::Session, None, Vec::new());
    usage.completeness = UsageCompleteness::Pending;
    let shown = view(Some(&usage));
    assert_eq!(usage_desired_row_count(&shown, 80), 2);
    assert!(row(&shown, 1, 2, 80).contains("Partial data"));
}

#[test]
fn session_activity_stays_visible_while_billing_is_pending() {
    let mut usage = report(UsageScope::Session, None, Vec::new());
    usage.completeness = UsageCompleteness::Pending;
    usage.session_activity = Some(ACTIVITY);
    let shown = view(Some(&usage));
    assert_eq!(usage_desired_row_count(&shown, 80), 5);
    assert_eq!(
        texts(&shown, 5, 80)[2..],
        [
            "Session activity",
            "API 1s · Wall Unavailable",
            "Code +5 · -2"
        ]
    );
}

#[test]
fn a_session_with_totals_shows_its_activity_between_the_overview_and_the_models() {
    let mut usage = pair();
    usage.scope = UsageScope::Session;
    usage.session_activity = Some(SessionActivity {
        code_complete: false,
        wall_duration_complete: true,
        wall_duration_ms: 3_725_000,
        ..ACTIVITY
    });
    let shown = view(Some(&usage));
    assert_eq!(usage_desired_row_count(&shown, 80), 14);
    assert_eq!(
        texts(&shown, 14, 80)[6..12],
        [
            "",
            "Session activity",
            "API 1s · Wall 1h 2m 5s",
            "Code activity unavailable",
            "",
            "Models 2",
        ]
    );
    assert_eq!(usage_desired_row_count(&shown, 40), 10);
    assert_eq!(
        texts(&shown, 10, 40)[4..7],
        [
            "Session activity",
            "API 1s · Wall 1h 2m 5s",
            "Code activity unavailable"
        ]
    );
}

#[test]
fn a_scope_without_models_says_so_and_the_status_names_partial_data() {
    let mut empty = report(UsageScope::Days7, Some(totals(0, 0.0)), Vec::new());
    let shown = view(Some(&empty));
    assert_eq!(usage_desired_row_count(&shown, 80), 9);
    assert_eq!(row(&shown, 8, 9, 80), "  No model usage in this scope.");
    empty.coverage = UsageCoverage::Partial;
    let partial = view(Some(&empty));
    assert_eq!(
        row(&partial, 1, 9, 80),
        "Partial data · some usage may be missing"
    );
    assert_eq!(row(&partial, 1, 9, 47), "Partial data");
    empty.coverage = UsageCoverage::NotStarted;
    empty.totals = None;
    assert_eq!(
        row(&view(Some(&empty)), 1, 2, 80),
        "Tracking has not started"
    );
    let failed = UsageMenuView {
        refresh_failed: true,
        ..view(Some(&empty))
    };
    assert_eq!(
        row(&failed, 1, 2, 80),
        "Refresh failed · showing previous data"
    );
}

#[test]
fn every_row_fits_its_width() {
    let mut usage = report(
        UsageScope::Session,
        Some(totals(1_234_656, 12.5)),
        vec![
            model("portkey/@openai/gpt-4o-with-a-long-name", 1_234_567, 12.5),
            model("codex/gpt-5-codex", 89, 0.0),
        ],
    );
    usage.session_activity = Some(ACTIVITY);
    let expanded = UsageMenuView {
        expanded_model: Some(0),
        ..view(Some(&usage))
    };
    for width in 1..=130 {
        for visible_rows in 1..=16 {
            for drawn in usage_menu_rows(&theme(), &expanded, visible_rows, width) {
                assert!(
                    drawn.width() <= width,
                    "{width}x{visible_rows}: {:?}",
                    drawn.text()
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
fn numbers_read_compactly_grouped_and_as_money_shares_and_durations() {
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
        [
            request_label(None),
            request_label(Some(1)),
            request_label(Some(1_464))
        ],
        ["Requests unavailable", "1 request", "1,464 requests"]
    );
    let durations: Vec<String> = [999, 61_000, 3_725_000]
        .into_iter()
        .map(format_duration)
        .collect();
    assert_eq!(durations, ["0s", "1m 1s", "1h 2m 5s"]);
    assert_eq!(
        float((1 << 40) + 3).to_bits(),
        1_099_511_627_779.0_f64.to_bits()
    );
}
