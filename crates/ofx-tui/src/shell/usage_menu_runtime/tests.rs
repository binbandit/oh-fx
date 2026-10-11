use ofx_contract::{
    ModelUsage, UiCommand, UiEvent, UsageCompleteness, UsageCoverage, UsageMenuUpdate, UsageReport,
    UsageScope, UsageTotals,
};

use crate::shell::test_shell::TestShell;

const HINT: &str = "tab scope     ↑↓ model     enter expand     r refresh     esc close";
const SHIFT_TAB: &[u8] = b"\x1b[Z";
const LEFT: &[u8] = b"\x1b[D";
const RIGHT: &[u8] = b"\x1b[C";
const DOWN: &[u8] = b"\x1b[B";

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

fn report(scope: UsageScope) -> Box<UsageReport> {
    let models = vec![
        ModelUsage {
            model: "portkey/@openai/gpt-4o".to_owned(),
            totals: totals(1_500, 0.25),
        },
        ModelUsage {
            model: "codex/gpt-5-codex".to_owned(),
            totals: totals(500, 0.0),
        },
    ];
    Box::new(UsageReport {
        scope,
        snapshot_time_ms: 100,
        window_start_ms: 0,
        coverage_started_at_ms: Some(0),
        coverage: UsageCoverage::Full,
        completeness: UsageCompleteness::Complete,
        totals: Some(totals(2_000, 0.25)),
        models,
        session_activity: None,
    })
}

fn update(update: UsageMenuUpdate) -> UiEvent {
    UiEvent::UsageMenu { update }
}

fn opened() -> TestShell {
    let mut test = TestShell::start();
    test.deliver(update(UsageMenuUpdate::Opened {
        scope: UsageScope::Days30,
        report: Some(report(UsageScope::Days30)),
    }));
    test
}

fn press(test: &mut TestShell, bytes: &[u8]) -> String {
    test.type_bytes(bytes);
    test.step();
    test.screen()
}

fn in_order(screen: &str, needles: &[&str]) {
    let mut from = 0;
    for needle in needles {
        let found = screen[from..]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {screen}"));
        from += found + needle.len();
    }
}

#[test]
fn the_menu_takes_the_composer_s_place_with_its_scopes_summary_and_models() {
    let mut test = opened();
    let screen = test.screen();
    in_order(
        &screen,
        &[
            "Usage  [30 days]  7 days  24 hours  Session",
            "Local oh-fx activity",
            "2K tokens · $0.25",
            "Models 2",
            "❯ portkey/@openai/gpt-4o",
            "1.5K · 75.0% · $0.25",
            "  codex/gpt-5-codex",
            "500 · 25.0% · $0.00",
            HINT,
        ],
    );
    assert!(!screen.contains('┃'), "{screen}");
    assert!(test.cursor_hidden());
}

#[test]
fn a_loading_menu_waits_for_its_report_and_a_failure_offers_a_retry() {
    let mut test = TestShell::start();
    test.deliver(update(UsageMenuUpdate::Opened {
        scope: UsageScope::Days30,
        report: None,
    }));
    assert!(test.screen().contains("Loading usage"));
    test.deliver(update(UsageMenuUpdate::Failed(UsageScope::Days30)));
    assert!(
        test.screen()
            .contains("Usage unavailable · press r to retry")
    );
    test.deliver(update(UsageMenuUpdate::Report(report(UsageScope::Days30))));
    let screen = test.screen();
    assert!(screen.contains("Local oh-fx activity"), "{screen}");
    test.deliver(update(UsageMenuUpdate::Failed(UsageScope::Days7)));
    let screen = test.screen();
    assert!(
        screen.contains("Refresh failed · showing previous data"),
        "{screen}"
    );
    assert!(screen.contains("[30 days]"), "{screen}");
}

#[test]
fn tab_and_shift_tab_cycle_every_scope_and_arrows_step_without_wrapping() {
    for (keys, scope) in [
        (&b"\t"[..], UsageScope::Days7),
        (SHIFT_TAB, UsageScope::Session),
        (LEFT, UsageScope::Days7),
    ] {
        let mut test = opened();
        press(&mut test, keys);
        assert_eq!(test.sent(), [UiCommand::RefreshUsage { scope }], "{keys:?}");
    }
    let mut test = opened();
    press(&mut test, RIGHT);
    assert!(test.sent().is_empty());

    let mut test = opened();
    for _ in 0..4 {
        press(&mut test, b"\t");
    }
    assert_eq!(
        test.sent(),
        [
            UiCommand::RefreshUsage {
                scope: UsageScope::Days7
            },
            UiCommand::RefreshUsage {
                scope: UsageScope::Hours24
            },
            UiCommand::RefreshUsage {
                scope: UsageScope::Session
            },
            UiCommand::RefreshUsage {
                scope: UsageScope::Days30
            },
        ]
    );
}

#[test]
fn r_reloads_the_scope_the_menu_navigates_and_a_report_moves_the_tabs() {
    let mut test = opened();
    press(&mut test, b"\t");
    test.deliver(update(UsageMenuUpdate::Loading(UsageScope::Days7)));
    let screen = test.screen();
    assert!(screen.contains("[7 days]"), "{screen}");
    assert!(screen.contains("Loading usage"), "{screen}");
    press(&mut test, b"r");
    assert_eq!(
        test.sent().last(),
        Some(&UiCommand::ReloadUsage {
            scope: UsageScope::Days7
        })
    );
    test.deliver(update(UsageMenuUpdate::Report(report(UsageScope::Days7))));
    assert!(
        test.screen()
            .contains("Usage  30 days  [7 days]  24 hours  Session")
    );
}

#[test]
fn enter_expands_the_selected_model_and_arrows_move_the_selection() {
    let mut test = opened();
    let screen = press(&mut test, DOWN);
    assert!(screen.contains("❯ codex/gpt-5-codex"), "{screen}");
    let screen = press(&mut test, b"\r");
    assert!(
        screen.contains("Input 500 · Output 0 · Cache 0/0 · Reasoning n/a · Requests 1 · $0.00"),
        "{screen}"
    );
    let screen = press(&mut test, b"\r");
    assert!(!screen.contains("Input 500"), "{screen}");
    assert!(test.sent().is_empty());
    assert!(test.shell.composer.is_empty());
}

#[test]
fn escape_closes_the_menu_and_later_reports_stay_closed() {
    let mut test = opened();
    press(&mut test, b"x \x0b");
    assert!(test.shell.composer.is_empty());
    press(&mut test, b"\x1b");
    test.advance(100);
    test.settle();
    assert!(test.shell.usage_menu.is_none());
    test.deliver(update(UsageMenuUpdate::Report(report(UsageScope::Days7))));
    test.deliver(update(UsageMenuUpdate::Failed(UsageScope::Days7)));
    assert!(test.shell.usage_menu.is_none());
    assert!(!test.screen().contains(HINT));
}

#[test]
fn the_usage_menu_and_the_other_compact_menus_replace_each_other() {
    let mut test = opened();
    test.deliver(UiEvent::StatuslineMenuOpened);
    assert!(test.shell.usage_menu.is_none());
    test.deliver(update(UsageMenuUpdate::Opened {
        scope: UsageScope::Days30,
        report: None,
    }));
    assert!(test.shell.statusline_menu.is_none());
    assert!(test.shell.usage_menu.is_some());
}

#[test]
fn a_late_answer_to_an_earlier_scope_leaves_the_newer_request_in_place() {
    for late in [
        UsageMenuUpdate::Loading(UsageScope::Days7),
        UsageMenuUpdate::Report(report(UsageScope::Days7)),
        UsageMenuUpdate::Failed(UsageScope::Days7),
    ] {
        let mut test = opened();
        press(&mut test, b"\t");
        press(&mut test, b"\t");
        test.deliver(update(late.clone()));
        press(&mut test, b"\t");
        assert_eq!(
            test.sent(),
            [
                UiCommand::RefreshUsage {
                    scope: UsageScope::Days7
                },
                UiCommand::RefreshUsage {
                    scope: UsageScope::Hours24
                },
                UiCommand::RefreshUsage {
                    scope: UsageScope::Session
                },
            ],
            "{late:?}"
        );
        test.deliver(update(UsageMenuUpdate::Report(report(UsageScope::Session))));
        assert!(
            test.screen()
                .contains("Usage  30 days  7 days  24 hours  [Session]"),
            "{late:?}"
        );
    }
}
