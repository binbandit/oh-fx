use std::sync::{Arc, Mutex};
use std::time::Duration;

use ofx_contract::{
    ModelFailureDiagnostic, ModelRecoveryAction, ModelRecoveryCause, RouteRecoveryKind,
    RouteRecoveryStatus, TurnId, TurnOutcome, UiEvent,
};

use super::super::test_shell::TestShell;
use crate::host::SteeringQueue;

const UP: &[u8] = b"\x1b[A";

struct Waiting(Arc<Mutex<Vec<(u64, String)>>>);

impl SteeringQueue for Waiting {
    fn retract_newest(&self) -> Option<(u64, String)> {
        self.0.lock().unwrap().pop()
    }
}

fn waiting(entries: &[(u64, &str)]) -> Arc<Mutex<Vec<(u64, String)>>> {
    Arc::new(Mutex::new(
        entries
            .iter()
            .map(|(prompt, text)| (*prompt, (*text).to_owned()))
            .collect(),
    ))
}

fn running(queue: &Arc<Mutex<Vec<(u64, String)>>>) -> TestShell {
    let shared = Arc::clone(queue);
    let mut test = TestShell::start_with(|options| {
        options.steering = Some(Box::new(Waiting(shared)));
    });
    test.submit("go");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    test
}

fn press(test: &mut TestShell, bytes: &[u8]) -> String {
    test.type_bytes(bytes);
    test.draining(|shell| assert!(shell.step().unwrap().is_none()));
    test.shell.composer.text().to_owned()
}

fn text(value: &str) -> UiEvent {
    UiEvent::AssistantText {
        turn_id: TurnId::new(1),
        text: value.to_owned(),
    }
}

fn recovery(status: RouteRecoveryStatus) -> UiEvent {
    UiEvent::Recovery {
        turn_id: TurnId::new(1),
        status,
    }
}

fn rate_limited(wait: Option<Duration>) -> RouteRecoveryStatus {
    RouteRecoveryStatus {
        kind: RouteRecoveryKind::AutoRetry,
        failed_attempt: 1,
        succeeded_attempt: 0,
        attempt_limit: 10,
        cause: Some(ModelRecoveryCause::RateLimited),
        action: Some(ModelRecoveryAction::RetryingRequest),
        delay_seconds: 60,
        diagnostic: Some(ModelFailureDiagnostic::new("HTTP 429 · slow")),
        retry_wait: wait,
    }
}

fn recovered() -> RouteRecoveryStatus {
    RouteRecoveryStatus {
        kind: RouteRecoveryKind::AutoRecovered,
        failed_attempt: 0,
        succeeded_attempt: 2,
        attempt_limit: 10,
        cause: None,
        action: None,
        delay_seconds: 0,
        diagnostic: None,
        retry_wait: None,
    }
}

fn steer(test: &mut TestShell, prompt: &str) {
    test.submit(prompt);
    test.deliver(UiEvent::SteeringApplied {
        turn_id: TurnId::new(1),
        prompt: 1,
        text: prompt.to_owned(),
    });
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
fn applied_steering_leaves_the_banner_and_joins_the_running_turn_as_a_user_row() {
    let mut test = running(&waiting(&[]));
    test.deliver(text("Looking at the parser"));
    test.submit("also check the tests");
    let screen = test.screen();
    assert!(screen.contains("┋ also check the tests"), "{screen}");
    test.deliver(UiEvent::SteeringApplied {
        turn_id: TurnId::new(1),
        prompt: 1,
        text: "also check the tests".to_owned(),
    });
    test.deliver(text("Checked them.\n"));
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(1),
        outcome: TurnOutcome::Completed,
    });
    let screen = test.screen();
    assert!(!screen.contains("┋"), "{screen}");
    assert!(
        screen.contains("  Looking at the parser\n\n┃ also check the tests\n\n  Checked them.\n\n"),
        "{screen}"
    );
    assert!(test.shell.outstanding.is_empty());
    assert!(test.shell.turn.is_none());
}

#[test]
fn steering_applied_to_a_hidden_turn_only_leaves_the_banner() {
    let mut test = running(&waiting(&[]));
    test.submit("late steer");
    test.deliver(UiEvent::SteeringApplied {
        turn_id: TurnId::new(9),
        prompt: 1,
        text: "late steer".to_owned(),
    });
    let screen = test.screen();
    assert!(!screen.contains("late steer"), "{screen}");
}

#[test]
fn steering_applied_after_a_local_cancel_retires_the_turn_promoted_for_it() {
    let mut test = running(&waiting(&[]));
    test.submit("steer B");
    press(&mut test, b"\x03");
    let screen = test.screen();
    assert!(screen.contains("┃ steer B"), "{screen}");
    assert!(test.shell.turn.is_some());
    test.deliver(UiEvent::SteeringApplied {
        turn_id: TurnId::new(1),
        prompt: 1,
        text: "steer B".to_owned(),
    });
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(1),
        outcome: TurnOutcome::Interrupted,
    });
    assert!(test.shell.turn.is_none());
    assert!(test.shell.outstanding.is_empty());
    test.submit("next prompt");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(2),
    });
    test.deliver(UiEvent::AssistantText {
        turn_id: TurnId::new(2),
        text: "Next answer.\n".to_owned(),
    });
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(2),
        outcome: TurnOutcome::Completed,
    });
    let screen = test.screen();
    assert!(
        screen.contains("┃ steer B\n\n┃ next prompt\n\n  Next answer.\n"),
        "{screen}"
    );
    assert!(test.shell.turn.is_none());
    assert!(test.shell.outstanding.is_empty());
}

#[test]
fn up_pulls_the_newest_waiting_steer_back_into_an_empty_composer() {
    let queue = waiting(&[(1, "first steer"), (2, "second steer")]);
    let mut test = running(&queue);
    test.submit("first steer");
    test.submit("second steer");
    assert_eq!(press(&mut test, UP), "second steer");
    let screen = test.screen();
    assert!(screen.contains("┋ first steer"), "{screen}");
    assert!(!screen.contains("┋ second steer"), "{screen}");
    assert_eq!(queue.lock().unwrap().len(), 1);
    assert_eq!(test.shell.outstanding.len(), 2);
    press(&mut test, UP);
    assert_eq!(queue.lock().unwrap().len(), 1);
}

#[test]
fn up_with_a_draft_leaves_the_steer_waiting() {
    let queue = waiting(&[(1, "steer")]);
    let mut test = running(&queue);
    test.submit("steer");
    assert_eq!(press(&mut test, b"draft"), "draft");
    assert_eq!(press(&mut test, UP), "draft");
    assert_eq!(queue.lock().unwrap().len(), 1);
    test.shell.composer.clear();
    assert_eq!(press(&mut test, UP), "steer");
    assert!(queue.lock().unwrap().is_empty());
}

#[test]
fn steering_that_ends_a_retry_wait_clears_the_retry_status() {
    let mut test = running(&waiting(&[]));
    test.deliver(recovery(rate_limited(Some(Duration::from_mins(1)))));
    assert!(test.screen().contains("retrying request in 60s"));
    steer(&mut test, "steer now");
    let screen = test.screen();
    assert!(!screen.contains("Rate limited"), "{screen}");
    assert!(screen.contains("Thinking"), "{screen}");
    assert!(screen.contains("┃ steer now"), "{screen}");
    test.deliver(text("Steered.\n"));
    let screen = after(&mut test, 61_000);
    assert!(!screen.contains("retrying request"), "{screen}");
    assert!(!screen.contains("recovered"), "{screen}");
}

#[test]
fn steering_during_a_retried_request_clears_its_retry_label() {
    let mut test = running(&waiting(&[]));
    test.deliver(recovery(rate_limited(None)));
    assert!(test.screen().contains("retrying request in 60s"));
    steer(&mut test, "steer now");
    let screen = test.screen();
    assert!(!screen.contains("retrying request"), "{screen}");
    assert!(screen.contains("Thinking"), "{screen}");
}

#[test]
fn steering_leaves_a_recovered_status_to_expire_on_its_own() {
    let mut test = running(&waiting(&[]));
    test.deliver(recovery(recovered()));
    steer(&mut test, "steer now");
    assert!(test.screen().contains("✓ recovered · attempt 2"));
    let screen = after(&mut test, 2_000);
    assert!(!screen.contains("recovered"), "{screen}");
    assert!(screen.contains("Thinking"), "{screen}");
}
