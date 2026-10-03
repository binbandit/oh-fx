use ofx_text::mask_secrets;
use serde_json::{Map, Value};

use crate::auto_classifier::ReviewFailure;
use crate::tool_args::parse_json_value;
use crate::types::{ToolArgumentDiagnostic, ToolArgumentFailure};

#[cfg(target_os = "macos")]
const FILESYSTEM_ACCESS_DENIED_SUGGESTION: &str = "Do not retry this path unchanged or propose a symlink. oh-fx permissions cannot override the operating system. If the path is in a protected folder such as Desktop, Documents, or Downloads, ask the user to grant the terminal app Files and Folders or Full Disk Access. Otherwise, ask the user to correct OS filesystem permissions or move/copy the project to an accessible location.";
#[cfg(not(target_os = "macos"))]
const FILESYSTEM_ACCESS_DENIED_SUGGESTION: &str = "Do not retry this path unchanged or propose a symlink. oh-fx permissions cannot override the operating system. Ask the user to correct OS filesystem permissions or move/copy the project to an accessible location.";

const USER_DENIED_MESSAGE: &str = "Permission denied by user";
const USER_DENIED_SUGGESTION: &str = "The tool did not run. Do not retry unchanged; explain the denial or use a safer allowed alternative.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermissionDenialReason {
    UserDenied,
    AutoDenied,
    ReviewCaution,
    ReviewEvidenceIncomplete,
    ReviewUnavailable,
    PolicyDenied,
    PermissionRequired,
}

impl ToolPermissionDenialReason {
    const ALL: [(Self, &'static str); 7] = [
        (Self::UserDenied, "user_denied"),
        (Self::AutoDenied, "auto_denied"),
        (Self::ReviewCaution, "review_caution"),
        (Self::ReviewEvidenceIncomplete, "review_evidence_incomplete"),
        (Self::ReviewUnavailable, "review_unavailable"),
        (Self::PolicyDenied, "policy_denied"),
        (Self::PermissionRequired, "permission_required"),
    ];

    fn is_review_hold(self) -> bool {
        matches!(
            self,
            Self::ReviewCaution | Self::ReviewEvidenceIncomplete | Self::ReviewUnavailable
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionFailure<'a> {
    pub tool_name: &'a str,
    pub message: &'a str,
    pub details: &'a [(&'a str, &'a str)],
    pub suggestion: Option<&'a str>,
}

pub(crate) fn pre_tool_use_blocked_json(tool_name: &str, reason: &str) -> String {
    tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: reason,
        details: &[],
        suggestion: Some(
            "Do not retry the same tool call unchanged. Adjust the request or use an allowed alternative.",
        ),
    })
}

pub fn tool_execution_failure_json(failure: &ExecutionFailure<'_>) -> String {
    let details = failure
        .details
        .iter()
        .map(|(name, value)| ((*name).to_owned(), masked(value)))
        .collect();
    failure_json(
        failure.tool_name,
        failure.message,
        details,
        failure.suggestion,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailValue<'a> {
    Text(&'a str),
    Unsigned(u64),
    Boolean(bool),
}

pub fn valued_execution_failure_json(
    tool_name: &str,
    message: &str,
    details: &[(&str, DetailValue<'_>)],
    suggestion: Option<&str>,
) -> String {
    let details = details
        .iter()
        .map(|(name, value)| {
            let value = match value {
                DetailValue::Text(text) => masked(text),
                DetailValue::Unsigned(number) => Value::from(*number),
                DetailValue::Boolean(flag) => Value::Bool(*flag),
            };
            ((*name).to_owned(), value)
        })
        .collect();
    failure_json(tool_name, message, details, suggestion)
}

pub fn malformed_tool_arguments_json(
    tool_name: &str,
    diagnostic: &ToolArgumentDiagnostic,
) -> String {
    let mut details = Map::new();
    details.insert("failure".to_owned(), masked(diagnostic.failure.name()));
    details.insert(
        "received_bytes".to_owned(),
        Value::from(diagnostic.input_bytes),
    );
    if let Some(offset) = diagnostic.error_offset {
        details.insert("error_offset".to_owned(), Value::from(offset));
    }
    let (message, suggestion) = match diagnostic.failure {
        ToolArgumentFailure::Truncated => (
            "Tool arguments ended before the JSON was complete, so oh-fx did not run the call. The conversation shows its arguments as {}.",
            "Reissue the complete call. Your arguments stopped after received_bytes; keep long arguments concise or split the work into smaller calls.",
        ),
        ToolArgumentFailure::SyntaxError => (
            "Tool arguments were not valid JSON, so oh-fx did not run the call. The conversation shows its arguments as {}.",
            "Reissue the call with valid JSON. Parsing failed at error_offset; escape quotes, backslashes, and newlines inside strings.",
        ),
        ToolArgumentFailure::RejectedValue => (
            "Tool arguments repeated an object key or held a value oh-fx cannot accept, so oh-fx did not run the call. The conversation shows its arguments as {}.",
            "Reissue the call with each object key used once and values matching the tool schema.",
        ),
    };
    failure_json(tool_name, message, details, Some(suggestion))
}

pub fn non_object_tool_arguments_json(tool_name: &str) -> String {
    tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: "Tool arguments must be a JSON object. The call was not executed.",
        details: &[],
        suggestion: Some("Reissue the tool call with a JSON object matching the tool schema."),
    })
}

fn failure_json(
    tool_name: &str,
    message: &str,
    details: Map<String, Value>,
    suggestion: Option<&str>,
) -> String {
    let mut error = Map::new();
    error.insert("type".to_owned(), Value::from("tool_execution_failed"));
    error.insert("tool_name".to_owned(), masked(tool_name));
    error.insert("message".to_owned(), masked(message));
    if !details.is_empty() {
        error.insert("details".to_owned(), Value::Object(details));
    }
    if let Some(suggestion) = suggestion {
        error.insert("suggestion".to_owned(), masked(suggestion));
    }
    let mut envelope = Map::new();
    envelope.insert("error".to_owned(), Value::Object(error));
    Value::Object(envelope).to_string()
}

pub fn format_tool_execution_error_json(tool_name: &str, error_name: &str) -> String {
    tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: "Tool execution failed",
        details: &[("error", error_name)],
        suggestion: None,
    })
}

pub fn filesystem_access_denied_json(tool_name: &str, path: &str, error_name: &str) -> String {
    tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: "Operating system denied filesystem access",
        details: &[("path", path), ("error", error_name)],
        suggestion: Some(FILESYSTEM_ACCESS_DENIED_SUGGESTION),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewHold<'a> {
    Caution(&'a str),
    EvidenceIncomplete,
    Unavailable(ReviewFailure),
}

impl ReviewHold<'_> {
    fn reason(self) -> &'static str {
        match self {
            Self::Caution(_) => "review_caution",
            Self::EvidenceIncomplete => "review_evidence_incomplete",
            Self::Unavailable(_) => "review_unavailable",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Caution(_) => "Action held after safety review",
            Self::EvidenceIncomplete => "Safety review evidence incomplete; action held",
            Self::Unavailable(failure) if failure.is_malformed_completion() => {
                "Safety reviewer returned an invalid response; action held"
            }
            Self::Unavailable(_) => "Safety reviewer unavailable; action held",
        }
    }

    fn suggestion(self) -> &'static str {
        match self {
            Self::Caution(_) => {
                "The action did not run. Use the review advice to choose a materially different safe action, or explain why no safe path remains."
            }
            Self::EvidenceIncomplete => {
                "The action did not run because safety review could not inspect the complete exact action. Do not retry unchanged; reduce the action or supporting evidence to fit the review limits, or choose a materially different fully inspectable action."
            }
            Self::Unavailable(failure) if failure.is_malformed_completion() => {
                "The action did not run because the reviewer did not return a valid decision. Continue with a different safe action or retry in a later turn."
            }
            Self::Unavailable(_) => {
                "The action did not run because safety review was unavailable. Continue with a different safe action or retry later."
            }
        }
    }
}

pub fn tool_review_held_json(tool_name: &str, hold: ReviewHold<'_>) -> String {
    let mut error = Map::new();
    error.insert("type".to_owned(), Value::from("tool_review_held"));
    error.insert("tool_name".to_owned(), Value::from(tool_name));
    error.insert("message".to_owned(), masked(hold.message()));
    error.insert("reason".to_owned(), Value::from(hold.reason()));
    if let ReviewHold::Unavailable(failure) = hold {
        error.insert("review_cause".to_owned(), Value::from(failure.as_str()));
    }
    error.insert("held".to_owned(), Value::Bool(true));
    if let ReviewHold::Caution(advice) = hold
        && !advice.is_empty()
    {
        error.insert("advice".to_owned(), masked(advice));
    }
    error.insert("suggestion".to_owned(), masked(hold.suggestion()));
    let mut envelope = Map::new();
    envelope.insert("error".to_owned(), Value::Object(error));
    Value::Object(envelope).to_string()
}

pub fn tool_permission_denied_json(tool_name: &str) -> String {
    let mut error = Map::new();
    error.insert("type".to_owned(), Value::from("tool_permission_denied"));
    error.insert("tool_name".to_owned(), masked(tool_name));
    error.insert("message".to_owned(), masked(USER_DENIED_MESSAGE));
    error.insert("reason".to_owned(), Value::from("user_denied"));
    error.insert("denied".to_owned(), Value::from(true));
    error.insert("suggestion".to_owned(), masked(USER_DENIED_SUGGESTION));
    let mut envelope = Map::new();
    envelope.insert("error".to_owned(), Value::Object(error));
    Value::Object(envelope).to_string()
}

pub fn tool_permission_denial_reason(output: &str) -> Option<ToolPermissionDenialReason> {
    let Some(Value::Object(root)) = parse_json_value(output) else {
        return None;
    };
    let error = root.get("error")?.as_object()?;
    let held = match error.get("type")?.as_str()? {
        "tool_review_held" => true,
        "tool_permission_denied" => false,
        _ => return None,
    };
    let name = error.get("reason")?.as_str()?;
    let (reason, _) = ToolPermissionDenialReason::ALL
        .into_iter()
        .find(|(_, candidate)| *candidate == name)?;
    (held == reason.is_review_hold()).then_some(reason)
}

pub fn shell_request_invalid_field_count(output: &str) -> Option<usize> {
    let Some(Value::Object(root)) = parse_json_value(output) else {
        return None;
    };
    let error = root.get("error")?.as_object()?;
    if error.get("code")?.as_str()? != "invalid_shell_request"
        || error.get("executed")?.as_bool()?
    {
        return None;
    }
    let problems = error.get("problems")?.as_array()?;
    (!problems.is_empty() && problems.iter().all(Value::is_string)).then_some(problems.len())
}

fn masked(text: &str) -> Value {
    Value::from(mask_secrets(text).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_failure_keeps_upstream_key_order() {
        let body = tool_execution_failure_json(&ExecutionFailure {
            tool_name: "read_file",
            message: "read_file failed",
            details: &[
                ("field", "path"),
                ("path", "missing.txt"),
                ("error", "FileNotFound"),
            ],
            suggestion: Some(
                "Run glob_files to discover matching paths, or check the path relative to the workspace.",
            ),
        });
        assert_eq!(
            body,
            "{\"error\":{\"type\":\"tool_execution_failed\",\"tool_name\":\"read_file\",\"message\":\"read_file failed\",\"details\":{\"field\":\"path\",\"path\":\"missing.txt\",\"error\":\"FileNotFound\"},\"suggestion\":\"Run glob_files to discover matching paths, or check the path relative to the workspace.\"}}"
        );
    }

    #[test]
    fn execution_failure_omits_empty_details_and_missing_suggestions() {
        assert_eq!(
            tool_execution_failure_json(&ExecutionFailure {
                tool_name: "grep_files",
                message: "grep_files failed",
                details: &[],
                suggestion: None,
            }),
            "{\"error\":{\"type\":\"tool_execution_failed\",\"tool_name\":\"grep_files\",\"message\":\"grep_files failed\"}}"
        );
    }

    #[test]
    fn valued_execution_failures_keep_numbers_and_flags_unquoted() {
        assert_eq!(
            valued_execution_failure_json(
                "web_fetch",
                "web_fetch received non-success HTTP status",
                &[
                    (
                        "url",
                        DetailValue::Text("https://example.com/?token=abcdefghijklmnop")
                    ),
                    ("status", DetailValue::Unsigned(404)),
                    ("body_truncated", DetailValue::Boolean(false)),
                ],
                None,
            ),
            "{\"error\":{\"type\":\"tool_execution_failed\",\"tool_name\":\"web_fetch\",\"message\":\"web_fetch received non-success HTTP status\",\"details\":{\"url\":\"https://example.com/?token=[redacted]\",\"status\":404,\"body_truncated\":false}}}"
        );
    }

    #[test]
    fn user_denials_match_upstream_permission_denied_results() {
        assert_eq!(
            tool_permission_denied_json("read_file"),
            "{\"error\":{\"type\":\"tool_permission_denied\",\"tool_name\":\"read_file\",\"message\":\"Permission denied by user\",\"reason\":\"user_denied\",\"denied\":true,\"suggestion\":\"The tool did not run. Do not retry unchanged; explain the denial or use a safer allowed alternative.\"}}"
        );
    }

    #[test]
    fn permission_denial_reasons_come_only_from_matching_denial_envelopes() {
        assert_eq!(
            tool_permission_denial_reason(&tool_permission_denied_json("read_file")),
            Some(ToolPermissionDenialReason::UserDenied)
        );
        assert_eq!(
            tool_permission_denial_reason(&tool_review_held_json(
                "edit_file",
                ReviewHold::Unavailable(ReviewFailure::ReviewerUnconfigured)
            )),
            Some(ToolPermissionDenialReason::ReviewUnavailable)
        );
        for (reason, name) in ToolPermissionDenialReason::ALL {
            let kind = if reason.is_review_hold() {
                "tool_review_held"
            } else {
                "tool_permission_denied"
            };
            let output = format!(r#"{{"error":{{"type":"{kind}","reason":"{name}"}}}}"#);
            assert_eq!(
                tool_permission_denial_reason(&output),
                Some(reason),
                "{name}"
            );
        }
        for output in [
            "",
            "user_denied",
            "[]",
            r#"{"error":"user_denied"}"#,
            r#"{"error":{"type":"tool_review_held","reason":"user_denied"}}"#,
            r#"{"error":{"type":"tool_review_held","reason":"auto_denied"}}"#,
            r#"{"error":{"type":"tool_permission_denied","reason":"review_unavailable"}}"#,
            r#"{"error":{"type":"tool_permission_denied","reason":"review_caution"}}"#,
            r#"{"error":{"type":"tool_permission_denied","reason":"unknown"}}"#,
            r#"{"error":{"type":"tool_execution_failed","reason":"user_denied"}}"#,
            r#"{"error":{"type":"tool_permission_denied","reason":7}}"#,
        ] {
            assert_eq!(tool_permission_denial_reason(output), None, "{output}");
        }
    }

    #[test]
    fn shell_request_corrections_count_their_problems() {
        let cases = [
            (
                r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":["a","b"]}}"#,
                Some(2),
            ),
            (
                r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":["a"]}}"#,
                Some(1),
            ),
            (
                r#"{"error":{"code":"invalid_shell_request","executed":true,"problems":["a"]}}"#,
                None,
            ),
            (
                r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":[]}}"#,
                None,
            ),
            (
                r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":[1]}}"#,
                None,
            ),
            (
                r#"{"error":{"code":"other","executed":false,"problems":["a"]}}"#,
                None,
            ),
            (r#"{"error":{"code":"invalid_shell_request"}}"#, None),
            ("not json", None),
        ];
        for (output, expected) in cases {
            assert_eq!(
                shell_request_invalid_field_count(output),
                expected,
                "{output}"
            );
        }
    }

    #[test]
    fn execution_errors_name_the_failed_operation() {
        assert_eq!(
            format_tool_execution_error_json("read_file", "SystemResources"),
            "{\"error\":{\"type\":\"tool_execution_failed\",\"tool_name\":\"read_file\",\"message\":\"Tool execution failed\",\"details\":{\"error\":\"SystemResources\"}}}"
        );
    }

    #[test]
    fn execution_failure_masks_secret_shaped_values() {
        let body = tool_execution_failure_json(&ExecutionFailure {
            tool_name: "read_file",
            message: "read_file failed",
            details: &[("path", "API_KEY=abcdefghijklmnop")],
            suggestion: None,
        });
        assert!(body.contains("\"path\":\"API_KEY=[redacted]\""), "{body}");
        assert!(!body.contains("abcdefghijklmnop"));
    }

    #[test]
    fn unavailable_reviews_hold_the_action_with_upstream_fields() {
        assert_eq!(
            tool_review_held_json(
                "edit_file",
                ReviewHold::Unavailable(ReviewFailure::ReviewerUnconfigured)
            ),
            "{\"error\":{\"type\":\"tool_review_held\",\"tool_name\":\"edit_file\",\"message\":\"Safety reviewer unavailable; action held\",\"reason\":\"review_unavailable\",\"review_cause\":\"reviewer_unconfigured\",\"held\":true,\"suggestion\":\"The action did not run because safety review was unavailable. Continue with a different safe action or retry later.\"}}"
        );
        assert_eq!(
            tool_review_held_json(
                "edit_file",
                ReviewHold::Unavailable(ReviewFailure::TransportTransient)
            ),
            "{\"error\":{\"type\":\"tool_review_held\",\"tool_name\":\"edit_file\",\"message\":\"Safety reviewer unavailable; action held\",\"reason\":\"review_unavailable\",\"review_cause\":\"transport_transient\",\"held\":true,\"suggestion\":\"The action did not run because safety review was unavailable. Continue with a different safe action or retry later.\"}}"
        );
    }

    #[test]
    fn held_reviews_report_caution_advice_incomplete_evidence_and_invalid_responses() {
        let caution: Value = serde_json::from_str(&tool_review_held_json(
            "edit_file",
            ReviewHold::Caution("Concrete injection"),
        ))
        .unwrap();
        assert_eq!(
            caution,
            serde_json::json!({"error": {
                "type": "tool_review_held",
                "tool_name": "edit_file",
                "message": "Action held after safety review",
                "reason": "review_caution",
                "held": true,
                "advice": "Concrete injection",
                "suggestion": "The action did not run. Use the review advice to choose a materially different safe action, or explain why no safe path remains."
            }})
        );
        let incomplete: Value = serde_json::from_str(&tool_review_held_json(
            "write_file",
            ReviewHold::EvidenceIncomplete,
        ))
        .unwrap();
        assert_eq!(incomplete["error"]["reason"], "review_evidence_incomplete");
        assert_eq!(
            incomplete["error"]["message"],
            "Safety review evidence incomplete; action held"
        );
        assert!(incomplete["error"].get("review_cause").is_none());
        assert!(incomplete["error"].get("advice").is_none());
        let malformed: Value = serde_json::from_str(&tool_review_held_json(
            "shell",
            ReviewHold::Unavailable(ReviewFailure::CompletionText),
        ))
        .unwrap();
        assert_eq!(
            malformed["error"]["message"],
            "Safety reviewer returned an invalid response; action held"
        );
        assert_eq!(malformed["error"]["review_cause"], "completion_text");
        assert_eq!(
            malformed["error"]["suggestion"],
            "The action did not run because the reviewer did not return a valid decision. Continue with a different safe action or retry in a later turn."
        );
    }

    #[test]
    fn held_review_advice_is_masked_and_left_out_when_empty() {
        let masked = tool_review_held_json("shell", ReviewHold::Caution("token=abcdefghijklmnop"));
        assert!(
            masked.contains("\"advice\":\"token=[redacted]\""),
            "{masked}"
        );
        let empty = tool_review_held_json("shell", ReviewHold::Caution(""));
        assert!(!empty.contains("\"advice\""), "{empty}");
    }

    #[test]
    fn filesystem_access_denied_names_the_path_and_error() {
        let body = filesystem_access_denied_json("glob_files", "/tmp/blocked", "AccessDenied");
        assert!(body.starts_with(
            "{\"error\":{\"type\":\"tool_execution_failed\",\"tool_name\":\"glob_files\",\"message\":\"Operating system denied filesystem access\",\"details\":{\"path\":\"/tmp/blocked\",\"error\":\"AccessDenied\"},\"suggestion\":\"Do not retry this path unchanged or propose a symlink."
        ));
    }

    #[test]
    fn malformed_tool_arguments_json_reports_the_diagnosis_without_source_bytes() {
        let raw = r#"{"request":{"task":"REJECTED_SOURCE_SENTINEL and more"#;
        let payload =
            malformed_tool_arguments_json("subagent", &ToolArgumentDiagnostic::diagnose(raw));
        assert!(!payload.contains("REJECTED_SOURCE_SENTINEL"));
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        let error = &parsed["error"];
        let message = error["message"].as_str().unwrap();
        assert!(message.contains("ended before the JSON was complete"));
        assert!(message.contains("{}"));
        assert_eq!(error["details"]["failure"], "truncated");
        assert_eq!(error["details"]["received_bytes"], raw.len());
        assert_eq!(error["details"]["error_offset"], raw.len());

        let rejected = malformed_tool_arguments_json(
            "read_file",
            &ToolArgumentDiagnostic::diagnose(r#"{"a":1,"a":2}"#),
        );
        let rejected: Value = serde_json::from_str(&rejected).unwrap();
        let details = &rejected["error"]["details"];
        assert_eq!(details["failure"], "rejected_value");
        assert!(details.get("error_offset").is_none());
    }

    #[test]
    fn rejected_arguments_produce_the_upstream_results_in_order() {
        let diagnosed = |raw: &str| {
            malformed_tool_arguments_json("read_file", &ToolArgumentDiagnostic::diagnose(raw))
        };
        assert_eq!(
            diagnosed(r#"{"path":"a.txt","offset":"#),
            r#"{"error":{"type":"tool_execution_failed","tool_name":"read_file","message":"Tool arguments ended before the JSON was complete, so oh-fx did not run the call. The conversation shows its arguments as {}.","details":{"failure":"truncated","received_bytes":25,"error_offset":25},"suggestion":"Reissue the complete call. Your arguments stopped after received_bytes; keep long arguments concise or split the work into smaller calls."}}"#
        );
        assert_eq!(
            diagnosed(r#"{"path":"a",}"#),
            r#"{"error":{"type":"tool_execution_failed","tool_name":"read_file","message":"Tool arguments were not valid JSON, so oh-fx did not run the call. The conversation shows its arguments as {}.","details":{"failure":"syntax_error","received_bytes":13,"error_offset":12},"suggestion":"Reissue the call with valid JSON. Parsing failed at error_offset; escape quotes, backslashes, and newlines inside strings."}}"#
        );
        assert_eq!(
            diagnosed(r#"{"path":"a.txt","path":"b.txt"}"#),
            r#"{"error":{"type":"tool_execution_failed","tool_name":"read_file","message":"Tool arguments repeated an object key or held a value oh-fx cannot accept, so oh-fx did not run the call. The conversation shows its arguments as {}.","details":{"failure":"rejected_value","received_bytes":31},"suggestion":"Reissue the call with each object key used once and values matching the tool schema."}}"#
        );
        assert_eq!(
            non_object_tool_arguments_json("read_file"),
            r#"{"error":{"type":"tool_execution_failed","tool_name":"read_file","message":"Tool arguments must be a JSON object. The call was not executed.","suggestion":"Reissue the tool call with a JSON object matching the tool schema."}}"#
        );
    }
}
