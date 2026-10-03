use std::sync::{Arc, Mutex};

use ofx_contract::{TurnId, TurnOutcome, UiEvent};

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
