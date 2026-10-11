use ofx_contract::{SessionActivity, UsageCompleteness, UsageCoverage, UsageTotals};

use super::*;

fn totals(tokens: u64) -> UsageTotals {
    UsageTotals {
        total_tokens: tokens,
        input_tokens: tokens,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        request_count: Some(1),
        total_cost: 0.0,
    }
}

fn model(name: &str, tokens: u64) -> ModelUsage {
    ModelUsage {
        model: name.to_owned(),
        totals: totals(tokens),
    }
}

fn report(scope: UsageScope, models: Vec<ModelUsage>) -> Box<UsageReport> {
    Box::new(UsageReport {
        scope,
        snapshot_time_ms: 100,
        window_start_ms: 0,
        coverage_started_at_ms: Some(0),
        coverage: UsageCoverage::Full,
        completeness: UsageCompleteness::Complete,
        totals: Some(totals(
            models.iter().map(|model| model.totals.total_tokens).sum(),
        )),
        models,
        session_activity: None,
    })
}

#[test]
fn a_refresh_keeps_the_selected_and_expanded_model_by_name() {
    let mut menu = UsageMenu::opened(report(
        UsageScope::Days30,
        vec![model("provider/a", 10), model("provider/b", 5)],
    ));
    assert!(menu.move_model(1, 16));
    assert!(menu.toggle_expanded(16));

    menu.replace(report(
        UsageScope::Hours24,
        vec![model("provider/b", 20), model("provider/c", 1)],
    ));
    assert_eq!(menu.selected_model, 0);
    assert_eq!(menu.expanded_model, Some(0));
    assert_eq!(menu.scope(), UsageScope::Hours24);
}

#[test]
fn failed_scopes_advance_navigation_while_the_last_report_stays_shown() {
    let mut menu = UsageMenu::loading(UsageScope::Days30);
    menu.record_refresh_failure(UsageScope::Days30);
    menu.record_refresh_failure(UsageScope::Days7);
    assert_eq!(menu.scope(), UsageScope::Days7);
    assert_eq!(menu.navigation_scope(), UsageScope::Days7);
    menu.record_refresh_failure(UsageScope::Hours24);
    assert_eq!(menu.scope(), UsageScope::Hours24);
    assert_eq!(menu.navigation_scope(), UsageScope::Hours24);

    menu.replace(report(UsageScope::Days30, Vec::new()));
    assert!(!menu.refresh_failed);
    menu.record_refresh_failure(menu.navigation_scope().previous());
    assert_eq!(menu.scope(), UsageScope::Days30);
    assert_eq!(menu.navigation_scope(), UsageScope::Days7);
    menu.record_refresh_failure(menu.navigation_scope().previous());
    assert_eq!(menu.scope(), UsageScope::Days30);
    assert_eq!(menu.navigation_scope(), UsageScope::Hours24);
    let session = menu.navigation_scope().previous();
    assert_eq!(session, UsageScope::Session);
    menu.replace(report(session, Vec::new()));
    assert_eq!(menu.scope(), UsageScope::Session);
    assert_eq!(menu.navigation_scope(), UsageScope::Session);
}

#[test]
fn a_menu_can_show_a_loading_scope_before_its_report_arrives() {
    let mut menu = UsageMenu::loading(UsageScope::Days30);
    assert_eq!(menu.navigation_scope(), UsageScope::Days30);
    assert!(menu.report.is_none());
    assert!(!menu.refresh_failed);

    menu.set_loading_scope(UsageScope::Days7);
    assert_eq!(menu.navigation_scope(), UsageScope::Days7);
}

#[test]
fn the_selected_session_model_stays_inside_the_rendered_window() {
    let models = (0..13)
        .map(|index| model(&format!("provider/model-{index}"), index + 1))
        .collect();
    let mut session = report(UsageScope::Session, models);
    session.session_activity = Some(SessionActivity {
        api_duration_complete: true,
        wall_duration_complete: true,
        code_complete: true,
        api_duration_ms: 1,
        wall_duration_ms: 2,
        lines_added: 3,
        lines_removed: 4,
    });
    let mut menu = UsageMenu::opened(session);
    for _ in 0..12 {
        assert!(menu.move_model(1, 32));
    }
    assert_eq!(menu.selected_model, 12);
    assert_eq!(menu.model_window_start, 0);
    assert!(!menu.move_model(1, 32));

    assert!(menu.toggle_expanded(32));
    assert_eq!(menu.model_window_start, 0);
    assert!(menu.move_model(-1, 32));
    assert_eq!(menu.selected_model, 11);
    assert_eq!(menu.model_window_start, 0);

    for _ in 0..11 {
        assert!(menu.move_model(-1, 4));
    }
    assert!(menu.move_model(1, 4));
    for _ in 0..5 {
        menu.move_model(1, 4);
    }
    assert_eq!((menu.selected_model, menu.model_window_start), (6, 3));
}
