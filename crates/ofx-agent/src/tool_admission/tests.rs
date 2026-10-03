use ofx_contract::ToolCall;

use super::*;

fn shell(id: &str, arguments: &str) -> ToolCall {
    ToolCall::new(id, SHELL_TOOL, arguments)
}

#[test]
fn shell_execution_failures_retain_independent_batch_identities() {
    let first = shell("first", r#"{"action":"run","command":"first"}"#);
    let second = shell("second", r#"{"action":"run","command":"second"}"#);
    let mut state = ShellExecutionFailureRetry::default();

    state.begin_batch();
    state.observe(&first.name, &first.arguments, ToolResultStatus::Failure);
    state.observe(&second.name, &second.arguments, ToolResultStatus::Failure);
    assert!(!state.finish_batch());
    state.begin_batch();
    state.observe(&first.name, &first.arguments, ToolResultStatus::Failure);
    state.observe(&second.name, &second.arguments, ToolResultStatus::Failure);
    assert!(state.finish_batch());

    let mut state = ShellExecutionFailureRetry::default();
    state.begin_batch();
    state.observe(&first.name, &first.arguments, ToolResultStatus::Failure);
    state.observe(&second.name, &second.arguments, ToolResultStatus::Success);
    assert!(!state.finish_batch());
    state.begin_batch();
    state.observe(&first.name, &first.arguments, ToolResultStatus::Failure);
    state.observe(&second.name, &second.arguments, ToolResultStatus::Success);
    assert!(state.finish_batch());
}

#[test]
fn only_failed_shell_calls_count_and_a_batch_without_them_starts_over() {
    let failing = shell("call", r#"{"command":"false"}"#);
    let other = ToolCall {
        name: "read_file".to_owned(),
        ..failing.clone()
    };
    let mut state = ShellExecutionFailureRetry::default();
    for (calls, stops) in [
        (vec![(&failing, ToolResultStatus::Failure)], false),
        (vec![(&failing, ToolResultStatus::Success)], false),
        (vec![(&failing, ToolResultStatus::Failure)], false),
        (vec![(&other, ToolResultStatus::Failure)], false),
        (vec![(&failing, ToolResultStatus::Failure)], false),
        (vec![(&failing, ToolResultStatus::Failure)], true),
    ] {
        state.begin_batch();
        for (call, status) in calls {
            state.observe(&call.name, &call.arguments, status);
        }
        assert_eq!(state.finish_batch(), stops);
    }
}

#[test]
fn arguments_are_compared_exactly_as_the_model_sent_them() {
    let mut state = ShellExecutionFailureRetry::default();
    state.begin_batch();
    state.observe(
        SHELL_TOOL,
        r#"{"command":"x","cwd":"y"}"#,
        ToolResultStatus::Failure,
    );
    assert!(!state.finish_batch());
    state.begin_batch();
    state.observe(
        SHELL_TOOL,
        r#"{"cwd":"y","command":"x"}"#,
        ToolResultStatus::Failure,
    );
    assert!(!state.finish_batch());
}

fn correction(problems: &[&str]) -> String {
    let problems = problems
        .iter()
        .map(|problem| format!("\"{problem}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"error":{{"code":"invalid_shell_request","executed":false,"problems":[{problems}]}}}}"#
    )
}

#[test]
fn shell_validation_retry_retains_independent_batch_corrections() {
    let first = correction(&["request.command is required."]);
    let second = correction(&["request.session_id is required."]);
    let call = shell("terminal-call", "{}");
    let mut state = ShellValidationRetry::default();

    state.begin_batch();
    state.observe(&call, &first);
    state.observe(&call, &second);
    state.observe(&call, &first);
    assert_eq!(state.0.current.len(), 2);
    assert!(!state.finish_batch());

    state.begin_batch();
    state.observe(&call, &second);
    state.observe(&call, "ordinary valid result");
    state.observe(&call, &first);
    assert_eq!(state.0.current.len(), 2);
    assert!(state.finish_batch());
}

#[test]
fn shell_request_corrections_stop_after_the_complete_repeated_batch() {
    let failure = correction(&["request.yield_time_ms must be an integer."]);
    let mut state = ShellValidationRetry::default();
    for (index, arguments) in [
        r#"{"command":"true","yield_time_ms":"1000"}"#,
        r#"{"yield_time_ms":"1000","command":"true"}"#,
    ]
    .into_iter()
    .enumerate()
    {
        let call = shell("invalid", arguments);
        state.begin_batch();
        state.observe(&call, &failure);
        state.observe(&call, "ordinary successful neighboring result");
        assert_eq!(state.finish_batch(), index == 1);
    }
}

#[test]
fn only_shell_corrections_count_as_validation_failures() {
    let failure = correction(&["request.command is required."]);
    let other = ToolCall {
        name: "read_file".to_owned(),
        ..shell("call", "{}")
    };
    let mut state = ShellValidationRetry::default();
    for _ in 0..2 {
        state.begin_batch();
        state.observe(&other, &failure);
        state.observe(&shell("call", "{}"), "invalid arguments");
        state.observe(
            &shell("call", "{}"),
            r#"{"error":{"code":"invalid_shell_request","executed":true,"problems":["a"]}}"#,
        );
        assert!(!state.finish_batch());
    }
}
