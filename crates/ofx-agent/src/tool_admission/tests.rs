use ofx_contract::ToolCallId;

use super::*;

fn shell(id: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: SHELL_TOOL.to_owned(),
        arguments: arguments.to_owned(),
    }
}

#[test]
fn shell_execution_failures_retain_independent_batch_identities() {
    let first = shell("first", r#"{"action":"run","command":"first"}"#);
    let second = shell("second", r#"{"action":"run","command":"second"}"#);
    let mut state = ShellExecutionFailureRetry::default();

    state.begin_batch();
    state.observe(&first, ToolResultStatus::Failure);
    state.observe(&second, ToolResultStatus::Failure);
    assert!(!state.finish_batch());
    state.begin_batch();
    state.observe(&first, ToolResultStatus::Failure);
    state.observe(&second, ToolResultStatus::Failure);
    assert!(state.finish_batch());

    let mut state = ShellExecutionFailureRetry::default();
    state.begin_batch();
    state.observe(&first, ToolResultStatus::Failure);
    state.observe(&second, ToolResultStatus::Success);
    assert!(!state.finish_batch());
    state.begin_batch();
    state.observe(&first, ToolResultStatus::Failure);
    state.observe(&second, ToolResultStatus::Success);
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
            state.observe(call, status);
        }
        assert_eq!(state.finish_batch(), stops);
    }
}

#[test]
fn arguments_are_compared_exactly_as_the_model_sent_them() {
    let mut state = ShellExecutionFailureRetry::default();
    state.begin_batch();
    state.observe(
        &shell("a", r#"{"command":"x","cwd":"y"}"#),
        ToolResultStatus::Failure,
    );
    assert!(!state.finish_batch());
    state.begin_batch();
    state.observe(
        &shell("b", r#"{"cwd":"y","command":"x"}"#),
        ToolResultStatus::Failure,
    );
    assert!(!state.finish_batch());
}
