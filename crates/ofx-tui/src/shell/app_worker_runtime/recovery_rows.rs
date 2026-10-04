use std::time::Duration;

use ofx_contract::{
    ModelFailureDiagnostic, ModelRecoveryAction, ModelRecoveryCause, ModelRecoveryRequiredAction,
    RouteRecoveryKind, RouteRecoveryStatus, TurnId, TurnOutcome, UiCommand, UiEvent,
};

use super::super::test_shell::TestShell;

const TURN: u64 = 1;

fn retrying(
    cause: ModelRecoveryCause,
    action: ModelRecoveryAction,
    delay_seconds: u64,
    wait: Option<Duration>,
) -> RouteRecoveryStatus {
    RouteRecoveryStatus {
        kind: RouteRecoveryKind::AutoRetry,
        failed_attempt: 1,
        succeeded_attempt: 0,
        attempt_limit: 10,
        cause: Some(cause),
        action: Some(action),
        required_action: ModelRecoveryRequiredAction::None,
        delay_seconds,
        diagnostic: (action == ModelRecoveryAction::RetryingRequest)
            .then(|| ModelFailureDiagnostic::new("HTTP 429 · slow")),
        retry_wait: wait,
    }
}

fn rate_limited(delay_seconds: u64, wait: Option<Duration>) -> RouteRecoveryStatus {
    retrying(
        ModelRecoveryCause::RateLimited,
        ModelRecoveryAction::RetryingRequest,
        delay_seconds,
        wait,
    )
}

fn recovered(attempt: usize) -> RouteRecoveryStatus {
    RouteRecoveryStatus {
        kind: RouteRecoveryKind::AutoRecovered,
        failed_attempt: 0,
        succeeded_attempt: attempt,
        attempt_limit: 10,
        cause: None,
        action: None,
        required_action: ModelRecoveryRequiredAction::None,
        delay_seconds: 0,
        diagnostic: None,
        retry_wait: None,
    }
}

fn stopped(failed_attempt: usize) -> RouteRecoveryStatus {
    RouteRecoveryStatus {
        kind: RouteRecoveryKind::TerminalProviderError,
        failed_attempt,
        succeeded_attempt: 0,
        attempt_limit: 10,
        cause: Some(ModelRecoveryCause::ProviderUnavailable),
        action: None,
        required_action: ModelRecoveryRequiredAction::None,
        delay_seconds: 0,
        diagnostic: Some(ModelFailureDiagnostic::new("HTTP 503 · overloaded")),
        retry_wait: None,
    }
}

fn recovery(turn: u64, status: RouteRecoveryStatus) -> UiEvent {
    UiEvent::Recovery {
        turn_id: TurnId::new(turn),
        status,
    }
}

fn running() -> TestShell {
    let mut test = TestShell::start();
    test.submit("go");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(TURN),
    });
    test
}

fn finished(outcome: TurnOutcome) -> UiEvent {
    UiEvent::TurnFinished {
        turn_id: TurnId::new(TURN),
        outcome,
    }
}

fn after(test: &mut TestShell, millis: u64) -> String {
    test.advance(millis);
    test.draining(|shell| {
        let now_ms = shell.now_ms();
        shell.refresh_recovery_status(now_ms);
    });
    test.screen()
}

#[test]
fn a_retry_wait_counts_down_in_place_of_the_turn_activity() {
    let mut test = running();
    test.deliver(recovery(
        TURN,
        rate_limited(3, Some(Duration::from_secs(3))),
    ));
    let screen = test.screen();
    assert!(
        screen.contains("⚠ Rate limited · HTTP 429 · slow · retrying request in 3s"),
        "{screen}"
    );
    assert!(!screen.contains("Thinking"), "{screen}");
    let screen = after(&mut test, 500);
    assert!(screen.contains("retrying request in 3s"), "{screen}");
    let screen = after(&mut test, 1_000);
    assert!(screen.contains("retrying request in 2s"), "{screen}");
    let screen = after(&mut test, 1_000);
    assert!(screen.contains("retrying request in 1s"), "{screen}");
    let screen = after(&mut test, 1_000);
    assert!(
        screen.contains("⚠ Rate limited · HTTP 429 · slow · retrying request\n")
            || screen.ends_with("⚠ Rate limited · HTTP 429 · slow · retrying request"),
        "{screen}"
    );
    assert!(!screen.contains("retrying request in"), "{screen}");
}

#[test]
fn the_loop_wakes_at_each_second_of_the_countdown() {
    let mut test = running();
    test.deliver(recovery(
        TURN,
        rate_limited(2, Some(Duration::from_secs(2))),
    ));
    let (started, first, later, due) = test.draining(|shell| {
        let now_ms = shell.now_ms();
        (
            now_ms,
            shell.next_deadline_ms(now_ms).unwrap(),
            shell.next_deadline_ms(now_ms + 1_100).unwrap(),
            shell.next_deadline_ms(now_ms + 5_000),
        )
    });
    assert!(
        first > started && first <= started + 1_000,
        "{started} {first}"
    );
    assert_eq!(later, first + 1_000);
    assert_eq!(due, None);
}

#[test]
fn a_retried_request_in_flight_keeps_its_label() {
    let mut test = running();
    test.deliver(recovery(TURN, rate_limited(2, None)));
    assert!(test.screen().contains("retrying request in 2s"));
    let screen = after(&mut test, 5_000);
    assert!(
        screen.contains("⚠ Rate limited · HTTP 429 · slow · retrying request in 2s"),
        "{screen}"
    );
    assert!(!screen.contains("Thinking"), "{screen}");
}

fn connectivity_wait() -> RouteRecoveryStatus {
    retrying(
        ModelRecoveryCause::ConnectivityLost,
        ModelRecoveryAction::WaitingForConnectivity,
        2,
        Some(Duration::from_secs(2)),
    )
}

#[test]
fn a_connectivity_wait_counts_down_and_offers_escape_to_pause() {
    let mut test = running();
    test.deliver(recovery(TURN, connectivity_wait()));
    let screen = test.screen();
    assert!(
        screen.contains("⚠ Connection lost · waiting for connection · 2s · esc to pause"),
        "{screen}"
    );
    let screen = after(&mut test, 500);
    assert!(
        screen.contains("waiting for connection · 2s · esc to pause"),
        "{screen}"
    );
    let screen = after(&mut test, 1_000);
    assert!(
        screen.contains("⚠ Connection lost · waiting for connection · 1s · esc to pause"),
        "{screen}"
    );
    let screen = after(&mut test, 1_000);
    assert!(
        screen.contains("⚠ Connection lost · waiting for connection · esc to pause"),
        "{screen}"
    );
}

fn escape(test: &mut TestShell) {
    test.type_bytes(b"\x1b");
    test.step();
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
}

fn pauses(test: &TestShell) -> usize {
    test.sent()
        .iter()
        .filter(|command| {
            **command
                == UiCommand::PauseRecovery {
                    turn_id: TurnId::new(TURN),
                }
        })
        .count()
}

#[test]
fn one_escape_pauses_a_connectivity_wait() {
    let mut test = running();
    test.deliver(recovery(TURN, connectivity_wait()));
    escape(&mut test);
    assert_eq!(pauses(&test), 1);
    escape(&mut test);
    escape(&mut test);
    assert_eq!(pauses(&test), 1);
    assert!(
        !test
            .sent()
            .iter()
            .any(|command| matches!(command, UiCommand::Cancel { .. }))
    );
    let screen = test.screen();
    assert!(!screen.contains("Cancelled"), "{screen}");
}

#[test]
fn escape_during_an_http_retry_wait_only_arms_the_interrupt() {
    let mut test = running();
    test.deliver(recovery(
        TURN,
        rate_limited(3, Some(Duration::from_secs(3))),
    ));
    escape(&mut test);
    assert_eq!(pauses(&test), 0);
    assert!(test.shell.gestures.escape_interrupt_armed());
}

#[test]
fn a_paused_turn_keeps_its_label_until_the_next_prompt() {
    let mut test = running();
    test.resize(24, 140);
    let mut paused = stopped(1);
    paused.cause = Some(ModelRecoveryCause::ConnectivityLost);
    paused.action = Some(ModelRecoveryAction::Paused);
    paused.diagnostic = Some(ModelFailureDiagnostic::new("ConnectionFailed"));
    test.deliver(recovery(TURN, paused));
    test.deliver(finished(TurnOutcome::Failed));
    let label = "⚠ Connection lost · ConnectionFailed · recovery paused after 1 attempt · send a new message when you're ready";
    let screen = test.screen();
    assert!(screen.contains(label), "{screen}");
    assert!(!screen.contains("Cancelled"), "{screen}");
    test.submit("again");
    let screen = test.screen();
    assert!(!screen.contains("recovery paused"), "{screen}");
}

#[test]
fn a_recovery_shows_briefly_before_the_turn_activity_returns() {
    let mut test = running();
    test.deliver(recovery(TURN, rate_limited(1, None)));
    test.deliver(recovery(TURN, recovered(2)));
    let screen = test.screen();
    assert!(screen.contains("✓ recovered · attempt 2"), "{screen}");
    assert!(!screen.contains("retrying request"), "{screen}");
    let screen = after(&mut test, 1_000);
    assert!(screen.contains("✓ recovered · attempt 2"), "{screen}");
    let screen = after(&mut test, 1_000);
    assert!(!screen.contains("recovered"), "{screen}");
    assert!(screen.contains("Thinking"), "{screen}");
}

#[test]
fn a_recovery_without_an_attempt_says_recovered_alone() {
    let mut test = running();
    test.deliver(recovery(TURN, recovered(0)));
    let screen = test.screen();
    assert!(screen.contains("✓ recovered"), "{screen}");
    assert!(!screen.contains("attempt"), "{screen}");
}

#[test]
fn the_status_ends_with_its_turn() {
    for outcome in [
        TurnOutcome::Completed,
        TurnOutcome::Interrupted,
        TurnOutcome::Failed,
    ] {
        let mut test = running();
        test.deliver(recovery(
            TURN,
            rate_limited(3, Some(Duration::from_secs(3))),
        ));
        test.deliver(recovery(TURN, recovered(2)));
        test.deliver(finished(outcome));
        let screen = test.screen();
        assert!(!screen.contains("recovered"), "{outcome:?}: {screen}");
        assert!(
            !screen.contains("retrying request"),
            "{outcome:?}: {screen}"
        );
    }
}

#[test]
fn a_status_for_a_turn_that_is_not_visible_is_ignored() {
    let mut test = running();
    test.deliver(recovery(9, rate_limited(3, Some(Duration::from_secs(3)))));
    let screen = test.screen();
    assert!(!screen.contains("retrying request"), "{screen}");
    assert!(screen.contains("Thinking"), "{screen}");
}

#[test]
fn a_narrow_footer_wraps_the_status_and_keeps_the_countdown() {
    let label = "⚠ Rate limited · HTTP 429 · slow · retrying request in 3s";
    let mut test = running();
    test.resize(24, 30);
    test.deliver(recovery(
        TURN,
        rate_limited(3, Some(Duration::from_secs(3))),
    ));
    let screen = test.screen();
    let rows = status_rows(&screen);
    assert_eq!(rows.len(), 3, "{screen}");
    assert_eq!(rows.join(" "), label, "{screen}");
    test.resize(24, 18);
    let screen = test.screen();
    let rows = status_rows(&screen);
    assert_eq!(rows.len(), 3, "{screen}");
    assert!(rows[2].starts_with("..."), "{screen}");
    assert!(rows[2].ends_with("request in 3s"), "{screen}");
}

fn status_rows(screen: &str) -> Vec<&str> {
    let lines: Vec<&str> = screen.lines().collect();
    let first = lines
        .iter()
        .position(|row| row.starts_with('⚠'))
        .expect("the status row");
    lines[first..]
        .iter()
        .take_while(|row| !row.trim().is_empty())
        .map(|row| row.trim())
        .collect()
}

const STOPPED: &str = "⚠ Provider unavailable · HTTP 503 · overloaded · stopped after 10 attempts";

fn stop_the_turn(test: &mut TestShell) {
    test.deliver(recovery(TURN, stopped(10)));
    test.deliver(UiEvent::ApiStatus {
        turn_id: TurnId::new(TURN),
        text: "⚠ API request failed · HTTP 503 · overloaded".to_owned(),
    });
    test.deliver(finished(TurnOutcome::Failed));
}

#[test]
fn a_stopped_turn_keeps_its_label_until_the_next_prompt_begins() {
    let mut test = running();
    test.resize(24, 100);
    stop_the_turn(&mut test);
    let screen = test.screen();
    assert!(screen.contains(STOPPED), "{screen}");
    assert!(
        screen.contains("⚠ API request failed · HTTP 503 · overloaded"),
        "{screen}"
    );
    let screen = after(&mut test, 60_000);
    assert!(screen.contains(STOPPED), "{screen}");
    test.submit("again");
    let screen = test.screen();
    assert!(!screen.contains(STOPPED), "{screen}");
    assert!(screen.contains("Thinking"), "{screen}");
}

#[test]
fn clearing_the_conversation_drops_a_kept_label() {
    let mut test = running();
    test.resize(24, 100);
    stop_the_turn(&mut test);
    assert!(test.screen().contains(STOPPED));
    test.deliver(UiEvent::ConversationCleared {
        first_kept_prompt: 1,
    });
    let screen = test.screen();
    assert!(!screen.contains(STOPPED), "{screen}");
}
