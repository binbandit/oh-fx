use ofx_contract::{Notice, NoticeTone, TurnId, TurnOutcome, UiCommand, UiEvent};

use crate::shell::SubmissionState;
use crate::shell::test_shell::TestShell;

const SIGNED_OUT: &str = "Codex needs a subscription login. Run /login, open Connections, then choose Codex subscription.";

fn press(test: &mut TestShell, keys: &[u8]) {
    test.type_bytes(keys);
    test.step();
}

fn hold(test: &mut TestShell, prompt: &str) {
    test.submit(prompt);
    test.deliver(UiEvent::Notice {
        notice: Notice::new(NoticeTone::Warning, "auth", SIGNED_OUT),
    });
    test.deliver(UiEvent::PromptHeld);
}

fn submitted(test: &TestShell) -> Vec<String> {
    test.sent()
        .into_iter()
        .filter_map(|command| match command {
            UiCommand::Submit { prompt, .. } => Some(prompt),
            _ => None,
        })
        .collect()
}

#[test]
fn a_prompt_held_for_a_login_keeps_its_card_and_waits_for_it() {
    let mut test = TestShell::start();
    hold(&mut test, "fix the tests");
    let screen = test.screen();
    assert_eq!(screen.matches("fix the tests").count(), 1, "{screen}");
    assert!(
        screen.contains("! auth: Codex needs a subscription login."),
        "{screen}"
    );
    assert!(!screen.contains("Thinking"), "{screen}");
    assert!(!screen.contains('┋'), "{screen}");
    press(&mut test, b"later\r");
    assert_eq!(test.shell.composer.text(), "later");
    assert_eq!(submitted(&test), ["fix the tests"]);
    press(&mut test, b"\x15\r");
    assert_eq!(test.sent().last(), Some(&UiCommand::RetryHeldPrompt));
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    let screen = test.screen();
    assert_eq!(screen.matches("fix the tests").count(), 1, "{screen}");
    assert!(screen.contains("Thinking"), "{screen}");
}

#[test]
fn ctrl_c_drops_a_held_prompt_and_keeps_the_draft() {
    let mut test = TestShell::start();
    hold(&mut test, "fix the tests");
    press(&mut test, b"draft");
    press(&mut test, b"\x03");
    assert_eq!(test.sent().last(), Some(&UiCommand::DropHeldPrompt));
    assert_eq!(test.shell.composer.text(), "draft");
    assert!(test.screen().contains("press ctrl+c again to exit"));
    press(&mut test, b"\r");
    assert_eq!(submitted(&test), ["fix the tests", "draft"]);
}

fn typed_ahead_of_a_sign_in(test: &mut TestShell) {
    press(test, b"/login cod\r");
    press(test, b"first\r");
    press(test, b"second\r");
    assert_eq!(submitted(test), ["first", "second"]);
    test.deliver(UiEvent::SignInStarted {
        url: "https://auth.example/oauth/authorize?state=s".to_owned(),
    });
    test.deliver(UiEvent::PromptHeld);
    test.deliver(UiEvent::PromptHeld);
}

fn states(test: &TestShell) -> Vec<(String, SubmissionState, Option<TurnId>)> {
    test.shell
        .outstanding
        .iter()
        .map(|submission| {
            (
                submission.prompt.clone(),
                submission.state,
                submission.turn_id,
            )
        })
        .collect()
}

#[test]
fn every_prompt_typed_ahead_of_a_sign_in_is_dropped_with_it() {
    for cancelled in [true, false] {
        let mut test = TestShell::start();
        typed_ahead_of_a_sign_in(&mut test);
        assert_eq!(
            states(&test),
            [
                ("first".to_owned(), SubmissionState::Held, None),
                ("second".to_owned(), SubmissionState::Held, None),
            ]
        );
        if cancelled {
            press(&mut test, b"\x1b[27u");
            assert_eq!(test.sent().last(), Some(&UiCommand::CancelSignIn));
        }
        test.deliver(UiEvent::SignInEnded);
        if !cancelled {
            test.deliver(UiEvent::Notice {
                notice: Notice::new(
                    NoticeTone::Error,
                    "auth",
                    "Codex sign-in failed. The current credential is unchanged.",
                ),
            });
        }
        test.deliver(UiEvent::HeldPromptDropped);
        assert!(test.shell.outstanding.is_empty(), "{:?}", states(&test));
        assert!(test.shell.turn.is_none());
        test.submit("next");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        assert_eq!(
            states(&test),
            [(
                "next".to_owned(),
                SubmissionState::Active,
                Some(TurnId::new(1))
            )]
        );
    }
}

#[test]
fn esc_drops_the_held_prompt_at_once_so_the_next_prompt_is_sent_and_held() {
    let mut test = TestShell::start();
    hold(&mut test, "fix the tests");
    test.deliver(UiEvent::SignInStarted {
        url: "https://auth.example/oauth/authorize?state=s".to_owned(),
    });
    press(&mut test, b"\x1b[27u");
    press(&mut test, b"again\r");
    assert_eq!(test.shell.composer.text(), "");
    assert_eq!(
        test.sent()[1..],
        [
            UiCommand::CancelSignIn,
            UiCommand::Submit {
                prompt: "again".to_owned(),
                skills: Vec::new(),
            },
        ]
    );
    test.deliver(UiEvent::SignInEnded);
    test.deliver(UiEvent::HeldPromptDropped);
    test.deliver(UiEvent::Notice {
        notice: Notice::new(NoticeTone::Warning, "auth", SIGNED_OUT),
    });
    test.deliver(UiEvent::PromptHeld);
    assert_eq!(
        states(&test),
        [("again".to_owned(), SubmissionState::Held, None)]
    );
    let screen = test.screen();
    assert_eq!(
        screen
            .matches("! auth: Codex needs a subscription login.")
            .count(),
        2,
        "{screen}"
    );
}

#[test]
fn prompts_typed_ahead_of_a_sign_in_start_in_order_once_it_switches() {
    let mut test = TestShell::start();
    typed_ahead_of_a_sign_in(&mut test);
    test.deliver(UiEvent::SignInEnded);
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    assert_eq!(
        states(&test),
        [
            (
                "first".to_owned(),
                SubmissionState::Active,
                Some(TurnId::new(1))
            ),
            ("second".to_owned(), SubmissionState::Held, None),
        ]
    );
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(1),
        outcome: TurnOutcome::Completed,
    });
    assert_eq!(
        states(&test),
        [("second".to_owned(), SubmissionState::Held, None)]
    );
    assert!(test.shell.turn.is_none());
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(2),
    });
    assert_eq!(
        states(&test),
        [(
            "second".to_owned(),
            SubmissionState::Active,
            Some(TurnId::new(2))
        )]
    );
}

#[test]
fn a_dropped_held_prompt_frees_the_composer() {
    let mut test = TestShell::start();
    hold(&mut test, "fix the tests");
    test.deliver(UiEvent::HeldPromptDropped);
    test.submit("again");
    assert_eq!(submitted(&test), ["fix the tests", "again"]);
}

#[test]
fn login_is_not_busy_while_a_prompt_waits_for_a_login() {
    let mut test = TestShell::start();
    hold(&mut test, "fix the tests");
    press(&mut test, b"/provider ");
    let screen = test.screen();
    assert!(screen.contains("portkey"), "{screen}");
}

#[test]
fn a_signed_out_session_asks_for_login_and_marks_no_provider_current() {
    let mut test = TestShell::start_with(|options| options.login_missing = true);
    assert!(test.screen().contains("run /login · auto · model-a"));
    press(&mut test, b"/provider ");
    let screen = test.screen();
    assert!(screen.contains("local"), "{screen}");
    assert!(!screen.contains("current"), "{screen}");
    press(&mut test, b"\x15");
    test.deliver(UiEvent::LoginChanged { missing: false });
    let screen = test.screen();
    assert!(!screen.contains("run /login"), "{screen}");
    assert!(screen.contains("auto · model-a"), "{screen}");
    test.deliver(UiEvent::LoginChanged { missing: true });
    assert!(test.screen().contains("run /login · auto · model-a"));
}
