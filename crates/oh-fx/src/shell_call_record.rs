use ofx_contract::parse_tool_args_object;
use serde::Serialize;
use serde_json::{Map, Value};

const SHELL_TOOL: &str = "shell";
const MAX_ERROR_CODE_BYTES: usize = 64;
const REVIEW_HOLD: &str = "tool_review_held";
const PERMISSION_DENIAL: &str = "tool_permission_denied";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CallError {
    category: &'static str,
    code: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellFailure {
    pub(crate) action: Option<&'static str>,
    pub(crate) error: CallError,
}

pub(crate) fn failed_call(
    tool_name: &str,
    arguments: &str,
    content: &str,
    command_result: Option<&str>,
) -> Option<ShellFailure> {
    if tool_name != SHELL_TOOL {
        return None;
    }
    if is_unexecuted_by_permission(content) {
        return rejected_call(tool_name, arguments);
    }
    let error = match command_result {
        Some(result) => CallError {
            category: "command_failed",
            code: command_failure_code(result).to_owned(),
        },
        None => CallError {
            category: "tool_failed",
            code: tool_failure_code(content),
        },
    };
    Some(ShellFailure {
        action: shell_action(arguments),
        error,
    })
}

pub(crate) fn rejected_call(tool_name: &str, arguments: &str) -> Option<ShellFailure> {
    unexecuted_call(tool_name, arguments, "rejected")
}

pub(crate) fn preflight_failed_call(tool_name: &str, arguments: &str) -> Option<ShellFailure> {
    unexecuted_call(tool_name, arguments, "tool_failed")
}

fn unexecuted_call(
    tool_name: &str,
    arguments: &str,
    outcome: &'static str,
) -> Option<ShellFailure> {
    (tool_name == SHELL_TOOL).then(|| ShellFailure {
        action: shell_action(arguments),
        error: CallError {
            category: outcome,
            code: outcome.to_owned(),
        },
    })
}

fn shell_action(arguments: &str) -> Option<&'static str> {
    parse_tool_args_object(arguments).ok()?;
    let root = object(arguments)?;
    let request = match root.get("request") {
        Some(Value::Object(request)) => request,
        Some(_) => return None,
        None => &root,
    };
    match request.get("action")?.as_str()? {
        "run" => Some("run"),
        "interact" => Some("interact"),
        "stop" => Some("stop"),
        _ => None,
    }
}

fn is_unexecuted_by_permission(content: &str) -> bool {
    object(content).is_some_and(|root| {
        matches!(
            root.get("error")
                .and_then(|failure| failure.get("type"))
                .and_then(Value::as_str),
            Some(REVIEW_HOLD | PERMISSION_DENIAL)
        )
    })
}

fn command_failure_code(command_result: &str) -> &'static str {
    let Some(result) = object(command_result) else {
        return "command_failed";
    };
    let flag = |name: &str| result.get(name).and_then(Value::as_bool) == Some(true);
    if flag("termination_indeterminate") {
        "termination_indeterminate"
    } else if flag("output_incomplete") {
        "output_incomplete"
    } else if flag("timed_out") {
        "timeout"
    } else if result.get("signal").is_some_and(|signal| !signal.is_null()) {
        "signal"
    } else if result
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
    {
        "nonzero_exit"
    } else {
        "command_failed"
    }
}

fn tool_failure_code(content: &str) -> String {
    let code = object(content).and_then(|root| {
        let failure = root.get("error")?.as_object()?;
        if failure.get("tool")?.as_str()? != SHELL_TOOL {
            return None;
        }
        failure
            .get("code")?
            .as_str()
            .filter(|code| is_safe_error_code(code))
            .map(str::to_owned)
    });
    code.unwrap_or_else(|| "tool_failed".to_owned())
}

fn is_safe_error_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= MAX_ERROR_CODE_BYTES
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn object(text: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str(text).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(action: Option<&'static str>, category: &'static str, code: &str) -> ShellFailure {
        ShellFailure {
            action,
            error: CallError {
                category,
                code: code.to_owned(),
            },
        }
    }

    #[test]
    fn shell_actions_come_from_the_request_or_the_bare_arguments() {
        for (arguments, action) in [
            (
                r#"{"request":{"action":"run","command":"ls"}}"#,
                Some("run"),
            ),
            (
                r#"{"action":"interact","session_id":"shell-1"}"#,
                Some("interact"),
            ),
            (
                r#"{"request":{"action":"stop"},"action":"run"}"#,
                Some("stop"),
            ),
            (r#"{"action":"legacy"}"#, None),
            (r#"{"request":"{\"action\":\"run\"}"}"#, None),
            (r#"{"action":7}"#, None),
            (r#"{"action":"run","action":"stop"}"#, None),
            ("[]", None),
            ("{", None),
        ] {
            assert_eq!(shell_action(arguments), action, "{arguments}");
        }
    }

    #[test]
    fn denied_and_held_shell_calls_are_recorded_as_rejected() {
        let arguments = r#"{"request":{"action":"run","command":"rm -rf build"}}"#;
        for content in [
            ofx_contract::tool_permission_denied_json("shell"),
            r#"{"error":{"type":"tool_review_held","tool_name":"shell"}}"#.to_owned(),
        ] {
            assert_eq!(
                failed_call("shell", arguments, &content, None),
                Some(failure(Some("run"), "rejected", "rejected")),
                "{content}"
            );
        }
    }

    #[test]
    fn command_failures_name_their_most_severe_cause() {
        for (result, code) in [
            (
                r#"{"termination_indeterminate":true,"output_incomplete":true}"#,
                "termination_indeterminate",
            ),
            (
                r#"{"output_incomplete":true,"timed_out":true}"#,
                "output_incomplete",
            ),
            (r#"{"timed_out":true,"signal":15}"#, "timeout"),
            (r#"{"signal":15,"exit_code":1}"#, "signal"),
            (r#"{"exit_code":7}"#, "nonzero_exit"),
            (r#"{"exit_code":0,"signal":null}"#, "command_failed"),
            ("not json", "command_failed"),
        ] {
            assert_eq!(command_failure_code(result), code, "{result}");
        }
    }

    #[test]
    fn tool_failures_keep_only_bounded_shell_error_codes() {
        for (content, code) in [
            (
                r#"{"error":{"tool":"shell","code":"ExecutionNotFound","retryable":false}}"#,
                "ExecutionNotFound",
            ),
            (
                r#"{"error":{"tool":"shell","code":"secret value"}}"#,
                "tool_failed",
            ),
            (
                r#"{"error":{"tool":"read_file","code":"Nope"}}"#,
                "tool_failed",
            ),
            ("shell run cwd is invalid: FileNotFound", "tool_failed"),
        ] {
            assert_eq!(tool_failure_code(content), code, "{content}");
        }
        let long = format!(
            r#"{{"error":{{"tool":"shell","code":"{}"}}}}"#,
            "a".repeat(65)
        );
        assert_eq!(tool_failure_code(&long), "tool_failed");
    }

    #[test]
    fn only_shell_calls_get_failure_details() {
        let run = r#"{"request":{"action":"run","command":"false"}}"#;
        assert_eq!(
            failed_call("shell", run, "{}", Some(r#"{"exit_code":1}"#)),
            Some(failure(Some("run"), "command_failed", "nonzero_exit"))
        );
        assert_eq!(
            failed_call("shell", run, "{}", None),
            Some(failure(Some("run"), "tool_failed", "tool_failed"))
        );
        assert_eq!(
            rejected_call("shell", run),
            Some(failure(Some("run"), "rejected", "rejected"))
        );
        assert_eq!(
            failed_call(
                "shell",
                run,
                &ofx_contract::tool_review_held_json(
                    "shell",
                    ofx_contract::ReviewHold::Unavailable(
                        ofx_contract::ReviewFailure::ReviewerUnconfigured
                    )
                ),
                None
            ),
            Some(failure(Some("run"), "rejected", "rejected"))
        );
        assert_eq!(
            preflight_failed_call("shell", run),
            Some(failure(Some("run"), "tool_failed", "tool_failed"))
        );
        assert_eq!(failed_call("read_file", "{}", "{}", None), None);
        assert_eq!(rejected_call("read_file", "{}"), None);
        assert_eq!(preflight_failed_call("read_file", "{}"), None);
    }
}
