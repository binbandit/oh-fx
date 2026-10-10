use std::sync::{Arc, Mutex};

use super::HookRuntime;
use crate::hooks::definitions::CONTEXT_BYTES;
use crate::hooks::{
    HookHandlerError, HookInvocation, HookScope, StopAction, StopInput, StopOutcome,
};
use crate::ids::TurnId;

fn input(can_continue: bool) -> StopInput<'static> {
    StopInput {
        invocation: HookInvocation {
            scope: HookScope::Interactive,
            turn_id: TurnId::new(42),
        },
        step_index: 3,
        assistant_text: "candidate",
        can_continue,
    }
}

fn single(
    action: impl Fn() -> Result<StopAction, HookHandlerError> + Send + Sync + 'static,
    can_continue: bool,
) -> StopOutcome {
    let mut runtime = HookRuntime::default();
    runtime.register_stop("single", move |_| action()).unwrap();
    runtime.freeze().run_stop(&input(can_continue))
}

#[test]
fn the_first_continuation_wins_and_later_handlers_do_not_run() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = HookRuntime::default();
    let seen = Arc::clone(&calls);
    runtime
        .register_stop("allow", move |input| {
            seen.lock()
                .unwrap()
                .push(("allow", input.assistant_text.to_owned(), input.step_index));
            Ok(StopAction::Allow)
        })
        .unwrap();
    let seen = Arc::clone(&calls);
    runtime
        .register_stop("continue", move |input| {
            seen.lock().unwrap().push((
                "continue",
                input.assistant_text.to_owned(),
                input.step_index,
            ));
            Ok(StopAction::ContinueOnce("continue".to_owned()))
        })
        .unwrap();
    let seen = Arc::clone(&calls);
    runtime
        .register_stop("never", move |_| {
            seen.lock().unwrap().push(("never", String::new(), 0));
            Ok(StopAction::Allow)
        })
        .unwrap();
    let view = runtime.freeze();
    assert!(view.has_stop());
    assert_eq!(
        view.run_stop(&input(true)),
        StopOutcome::ContinueOnce("continue".to_owned())
    );
    assert_eq!(
        *calls.lock().unwrap(),
        [
            ("allow", "candidate".to_owned(), 3),
            ("continue", "candidate".to_owned(), 3),
        ]
    );
    assert_eq!(view.run_stop(&input(false)), StopOutcome::Allow);
}

#[test]
fn handlers_that_allow_let_the_turn_finish() {
    assert_eq!(single(|| Ok(StopAction::Allow), true), StopOutcome::Allow);
}

#[test]
fn handler_errors_fail_open_and_stop_the_dispatch() {
    for error in [HookHandlerError::Failed, HookHandlerError::Cancelled] {
        let later = Arc::new(Mutex::new(false));
        let mut runtime = HookRuntime::default();
        runtime.register_stop("error", move |_| Err(error)).unwrap();
        let called = Arc::clone(&later);
        runtime
            .register_stop("later", move |_| {
                *called.lock().unwrap() = true;
                Ok(StopAction::ContinueOnce("never".to_owned()))
            })
            .unwrap();
        assert_eq!(runtime.freeze().run_stop(&input(true)), StopOutcome::Allow);
        assert!(!*later.lock().unwrap());
    }
}

#[test]
fn an_oversized_continuation_fails_open_and_the_limit_itself_continues() {
    assert_eq!(
        single(
            || Ok(StopAction::ContinueOnce("x".repeat(CONTEXT_BYTES + 1))),
            true
        ),
        StopOutcome::Allow
    );
    assert_eq!(
        single(
            || Ok(StopAction::ContinueOnce("x".repeat(CONTEXT_BYTES))),
            true
        ),
        StopOutcome::ContinueOnce("x".repeat(CONTEXT_BYTES))
    );
    assert_eq!(
        single(|| Ok(StopAction::ContinueOnce(String::new())), true),
        StopOutcome::ContinueOnce(String::new())
    );
}

#[test]
fn a_continuation_without_budget_becomes_an_allow() {
    assert_eq!(
        single(|| Ok(StopAction::ContinueOnce("verify".to_owned())), false),
        StopOutcome::Allow
    );
}
