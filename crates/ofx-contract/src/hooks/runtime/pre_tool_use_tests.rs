use std::sync::{Arc, Mutex};

use super::HookRuntime;
use crate::hooks::definitions::{ARGUMENTS_JSON_BYTES, REASON_BYTES};
use crate::hooks::{
    HookDispatchError, HookHandlerError, HookInvocation, HookScope, PreToolUseAction,
    PreToolUseInput, PreToolUseOutcome,
};
use crate::ids::TurnId;

fn input(arguments_json: &str) -> PreToolUseInput<'_> {
    PreToolUseInput {
        invocation: HookInvocation {
            scope: HookScope::Interactive,
            turn_id: TurnId::new(42),
        },
        step_index: 2,
        call_id: "call-1",
        tool_name: "read_file",
        arguments_json,
    }
}

fn single(
    action: impl Fn() -> Result<PreToolUseAction, HookHandlerError> + Send + Sync + 'static,
) -> Result<PreToolUseOutcome, HookDispatchError> {
    let mut runtime = HookRuntime::default();
    runtime
        .register_pre_tool_use("single", move |_| action())
        .unwrap();
    runtime.freeze().run_pre_tool_use(&input("{}"))
}

#[test]
fn rewrites_chain_in_registration_order_and_the_first_block_wins() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = HookRuntime::default();
    let seen = Arc::clone(&order);
    runtime
        .register_pre_tool_use("first", move |_| {
            seen.lock().unwrap().push(("first", String::new()));
            Ok(PreToolUseAction::RewriteArguments(r#"{"a":1}"#.to_owned()))
        })
        .unwrap();
    let seen = Arc::clone(&order);
    runtime
        .register_pre_tool_use("second", move |input| {
            seen.lock()
                .unwrap()
                .push(("second", input.arguments_json.to_owned()));
            Ok(PreToolUseAction::RewriteArguments(r#"{"b":2}"#.to_owned()))
        })
        .unwrap();
    let seen = Arc::clone(&order);
    runtime
        .register_pre_tool_use("block", move |input| {
            seen.lock()
                .unwrap()
                .push(("block", input.arguments_json.to_owned()));
            if input.arguments_json != r#"{"b":2}"# {
                return Err(HookHandlerError::Failed);
            }
            Ok(PreToolUseAction::Block("blocked by policy".to_owned()))
        })
        .unwrap();
    let seen = Arc::clone(&order);
    runtime
        .register_pre_tool_use("never", move |_| {
            seen.lock().unwrap().push(("never", String::new()));
            Ok(PreToolUseAction::Continue)
        })
        .unwrap();
    let outcome = runtime
        .freeze()
        .run_pre_tool_use(&input(r#"{"original":true}"#));
    assert_eq!(
        outcome,
        Ok(PreToolUseOutcome::Blocked("blocked by policy".to_owned()))
    );
    assert_eq!(
        *order.lock().unwrap(),
        [
            ("first", String::new()),
            ("second", r#"{"a":1}"#.to_owned()),
            ("block", r#"{"b":2}"#.to_owned()),
        ]
    );
}

#[test]
fn handlers_see_the_call_and_the_last_rewrite_is_the_outcome() {
    let seen = Arc::new(Mutex::new(None));
    let mut runtime = HookRuntime::default();
    let recorded = Arc::clone(&seen);
    runtime
        .register_pre_tool_use("observe", move |input| {
            *recorded.lock().unwrap() = Some((
                input.invocation,
                input.step_index,
                input.call_id.to_owned(),
                input.tool_name.to_owned(),
            ));
            Ok(PreToolUseAction::Continue)
        })
        .unwrap();
    runtime
        .register_pre_tool_use("rewrite", |_| {
            Ok(PreToolUseAction::RewriteArguments(
                r#" {"x":1} "#.to_owned(),
            ))
        })
        .unwrap();
    let view = runtime.freeze();
    assert_eq!(
        view.run_pre_tool_use(&input("{}")),
        Ok(PreToolUseOutcome::Rewritten(r#" {"x":1} "#.to_owned()))
    );
    assert_eq!(
        *seen.lock().unwrap(),
        Some((
            input("{}").invocation,
            2,
            "call-1".to_owned(),
            "read_file".to_owned()
        ))
    );
}

#[test]
fn handlers_that_only_continue_leave_the_call_unchanged() {
    assert_eq!(
        single(|| Ok(PreToolUseAction::Continue)),
        Ok(PreToolUseOutcome::Unchanged)
    );
}

#[test]
fn rewrites_must_be_one_json_object_within_the_size_limit() {
    for output in ["[]", "1", "null", "{", "{} trailing", r#"{"a":1,"a":2}"#] {
        assert_eq!(
            single(move || Ok(PreToolUseAction::RewriteArguments(output.to_owned()))),
            Err(HookDispatchError::InvalidHandlerOutput),
            "{output}"
        );
    }
    let oversized = " ".repeat(ARGUMENTS_JSON_BYTES + 1);
    assert_eq!(
        single(move || Ok(PreToolUseAction::RewriteArguments(oversized.clone()))),
        Err(HookDispatchError::HandlerOutputTooLarge)
    );
    let largest = format!("{{\"a\":\"{}\"}}", "x".repeat(ARGUMENTS_JSON_BYTES - 8));
    assert_eq!(largest.len(), ARGUMENTS_JSON_BYTES);
    let expected = largest.clone();
    assert_eq!(
        single(move || Ok(PreToolUseAction::RewriteArguments(largest.clone()))),
        Ok(PreToolUseOutcome::Rewritten(expected))
    );
}

#[test]
fn block_reasons_must_be_present_and_within_the_size_limit() {
    assert_eq!(
        single(|| Ok(PreToolUseAction::Block(String::new()))),
        Err(HookDispatchError::InvalidHandlerOutput)
    );
    assert_eq!(
        single(|| Ok(PreToolUseAction::Block("x".repeat(REASON_BYTES + 1)))),
        Err(HookDispatchError::HandlerOutputTooLarge)
    );
    assert_eq!(
        single(|| Ok(PreToolUseAction::Block("x".repeat(REASON_BYTES)))),
        Ok(PreToolUseOutcome::Blocked("x".repeat(REASON_BYTES)))
    );
}

#[test]
fn handler_errors_stop_the_dispatch_and_drop_earlier_rewrites() {
    assert_eq!(
        single(|| Err(HookHandlerError::Failed)),
        Err(HookDispatchError::HandlerFailed)
    );
    assert_eq!(
        single(|| Err(HookHandlerError::Cancelled)),
        Err(HookDispatchError::Cancelled)
    );
    let mut runtime = HookRuntime::default();
    runtime
        .register_pre_tool_use("rewrite", |_| {
            Ok(PreToolUseAction::RewriteArguments(
                r#"{"ok":true}"#.to_owned(),
            ))
        })
        .unwrap();
    runtime
        .register_pre_tool_use("error", |_| Err(HookHandlerError::Failed))
        .unwrap();
    assert_eq!(
        runtime.freeze().run_pre_tool_use(&input("{}")),
        Err(HookDispatchError::HandlerFailed)
    );
}

#[test]
fn an_invalid_output_stops_later_handlers() {
    let later = Arc::new(Mutex::new(false));
    let mut runtime = HookRuntime::default();
    runtime
        .register_pre_tool_use("invalid", |_| {
            Ok(PreToolUseAction::RewriteArguments("[]".to_owned()))
        })
        .unwrap();
    let called = Arc::clone(&later);
    runtime
        .register_pre_tool_use("later", move |_| {
            *called.lock().unwrap() = true;
            Ok(PreToolUseAction::Continue)
        })
        .unwrap();
    assert_eq!(
        runtime.freeze().run_pre_tool_use(&input("{}")),
        Err(HookDispatchError::InvalidHandlerOutput)
    );
    assert!(!*later.lock().unwrap());
}
