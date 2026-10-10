use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use super::{HookRuntime, HookView};
use crate::hooks::definitions::HANDLER_NAME_BYTES;
use crate::hooks::{
    AttentionKind, AttentionRequiredInput, HookInvocation, HookRegistrationError, HookScope,
    PostTurnEndInput,
};
use crate::ids::TurnId;
use crate::types::TurnPresentationOutcome;

fn turn_end(scope: HookScope, outcome: TurnPresentationOutcome) -> PostTurnEndInput {
    PostTurnEndInput {
        invocation: HookInvocation {
            scope,
            turn_id: TurnId::new(42),
        },
        outcome,
    }
}

fn attention(kind: AttentionKind) -> AttentionRequiredInput {
    AttentionRequiredInput {
        invocation: HookInvocation {
            scope: HookScope::Interactive,
            turn_id: TurnId::new(42),
        },
        kind,
    }
}

#[test]
fn registration_validates_names_and_refuses_duplicates_within_an_event() {
    let mut runtime = HookRuntime::default();
    let noop = |_: &AttentionRequiredInput| {};
    assert_eq!(
        runtime.register_attention_required("", noop),
        Err(HookRegistrationError::EmptyHandlerName)
    );
    assert_eq!(
        runtime.register_attention_required("bad name", noop),
        Err(HookRegistrationError::InvalidHandlerName)
    );
    assert_eq!(
        runtime.register_attention_required("caf\u{e9}", noop),
        Err(HookRegistrationError::InvalidHandlerName)
    );
    assert_eq!(
        runtime.register_attention_required(&"a".repeat(HANDLER_NAME_BYTES + 1), noop),
        Err(HookRegistrationError::HandlerNameTooLong)
    );
    assert_eq!(
        runtime.register_attention_required(&"a".repeat(HANDLER_NAME_BYTES), noop),
        Ok(())
    );
    assert_eq!(
        runtime.register_attention_required("fx.herdr_attention-1", noop),
        Ok(())
    );
    assert_eq!(
        runtime.register_attention_required("fx.herdr_attention-1", noop),
        Err(HookRegistrationError::DuplicateHandlerName)
    );
    assert!(runtime.freeze().has_attention_required());
}

#[test]
fn registration_errors_keep_upstream_names() {
    let names = [
        HookRegistrationError::EmptyHandlerName,
        HookRegistrationError::HandlerNameTooLong,
        HookRegistrationError::InvalidHandlerName,
        HookRegistrationError::DuplicateHandlerName,
    ]
    .map(|error| error.to_string());
    assert_eq!(
        names,
        [
            "EmptyHandlerName",
            "HandlerNameTooLong",
            "InvalidHandlerName",
            "DuplicateHandlerName"
        ]
    );
}

#[test]
fn a_name_may_be_registered_once_for_each_event() {
    let mut runtime = HookRuntime::default();
    assert_eq!(runtime.register_post_turn_end("alpha", |_| {}), Ok(()));
    assert_eq!(
        runtime.register_post_turn_end("alpha", |_| {}),
        Err(HookRegistrationError::DuplicateHandlerName)
    );
    assert_eq!(
        runtime.register_post_turn_end("bad name", |_| {}),
        Err(HookRegistrationError::InvalidHandlerName)
    );
    assert_eq!(runtime.register_attention_required("alpha", |_| {}), Ok(()));
    let view = runtime.freeze();
    assert!(view.has_post_turn_end());
    assert!(view.has_attention_required());
}

#[test]
fn a_view_reports_only_the_events_that_have_handlers() {
    let mut turn_end_only = HookRuntime::default();
    turn_end_only
        .register_post_turn_end("turn_end", |_| {})
        .unwrap();
    let view = turn_end_only.freeze();
    assert!(view.has_post_turn_end());
    assert!(!view.has_attention_required());
    let mut attention_only = HookRuntime::default();
    attention_only
        .register_attention_required("attention", |_| {})
        .unwrap();
    let view = attention_only.freeze();
    assert!(!view.has_post_turn_end());
    assert!(view.has_attention_required());
}

#[test]
fn empty_views_dispatch_nothing() {
    for view in [HookRuntime::default().freeze(), HookView::default()] {
        assert!(!view.has_post_turn_end());
        assert!(!view.has_attention_required());
        view.run_post_turn_end(&turn_end(
            HookScope::Interactive,
            TurnPresentationOutcome::Completed,
        ));
        view.run_attention_required(&attention(AttentionKind::Permission));
    }
}

#[test]
fn post_turn_end_runs_every_handler_in_registration_order_with_the_input() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = HookRuntime::default();
    for name in ["first", "second", "third"] {
        let seen = Arc::clone(&seen);
        runtime
            .register_post_turn_end(name, move |input| {
                seen.lock().unwrap().push((name, *input));
            })
            .unwrap();
    }
    let view = runtime.freeze();
    let input = turn_end(HookScope::Subagent, TurnPresentationOutcome::Paused);
    view.run_post_turn_end(&input);
    view.run_attention_required(&attention(AttentionKind::Question));
    assert_eq!(
        *seen.lock().unwrap(),
        [("first", input), ("second", input), ("third", input)]
    );
}

#[test]
fn attention_required_runs_every_handler_in_registration_order_with_the_input() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = HookRuntime::default();
    for name in ["first", "second", "third"] {
        let seen = Arc::clone(&seen);
        runtime
            .register_attention_required(name, move |input| {
                seen.lock().unwrap().push((name, *input));
            })
            .unwrap();
    }
    let view = runtime.freeze();
    view.run_attention_required(&attention(AttentionKind::RouteRecovery));
    let input = attention(AttentionKind::RouteRecovery);
    assert_eq!(
        *seen.lock().unwrap(),
        [("first", input), ("second", input), ("third", input)]
    );
}

#[test]
fn a_frozen_view_is_safe_to_share_across_concurrent_invocations() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut runtime = HookRuntime::default();
    let counted = Arc::clone(&calls);
    runtime
        .register_attention_required("concurrent", move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    let view = runtime.freeze();
    let runs = [view.clone(), view].map(|view| {
        thread::spawn(move || view.run_attention_required(&attention(AttentionKind::Question)))
    });
    for run in runs {
        run.join().unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
