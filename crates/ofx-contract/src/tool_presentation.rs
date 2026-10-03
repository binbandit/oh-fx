use ofx_text::{encode_terminal_safe, is_posix_space};
use serde_json::{Map, Value};

use crate::subagent::SteeringDelivery;
use crate::tool_args::parse_tool_args_object;
use crate::tool_dispatch::{
    ActionLabel, CallDescription, CallPresentation, Concurrency, ToolEffect,
};

const SUBAGENT_TOOL_NAME: &str = "subagent";
const SUBAGENT_NAME_BYTES: usize = 64;
const SUBAGENT_PREVIEW_BUFFER_BYTES: usize = 124;
const SUBAGENT_PREVIEW_BYTES: usize = 120;
const SUBAGENT_PREVIEW_SCAN_BYTES: usize = 16 * 1024;
const NOT_SENT_CODES: [&str; 3] = [
    "feedback_capacity",
    "operation_conflict",
    "override_after_create",
];
const INTERRUPTED_CODES: [&str; 3] = ["child_cancelled", "child_interrupted", "child_lost"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentActionState<'a> {
    Identity,
    Active,
    Pending,
    Completed,
    Feedback(SteeringDelivery),
    Stopped(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentActionText {
    pub label: String,
    pub detail: String,
}

pub fn plain_description(
    tool_name: &str,
    presentation: &CallPresentation,
    arguments: &str,
    effect: ToolEffect,
) -> CallDescription {
    let label = plain_action_label(presentation, arguments);
    CallDescription {
        title: format_plain_action(tool_name, label.as_ref()),
        label,
        activity: presentation.activity,
        effect,
        concurrency: Concurrency::Parallel,
    }
}

fn plain_action_label(presentation: &CallPresentation, arguments: &str) -> Option<ActionLabel> {
    let arguments = parse_tool_args_object(arguments).ok()?;
    let value = arguments
        .optional_string(presentation.label_argument)
        .unwrap_or(presentation.label_default);
    Some(presentation.label(value))
}

pub fn format_plain_action(tool_name: &str, label: Option<&ActionLabel>) -> String {
    label.map_or_else(|| format_unknown_action(tool_name), ActionLabel::title)
}

pub fn format_unknown_action(tool_name: &str) -> String {
    format!("Working: {tool_name}")
}

pub fn subagent_action(
    tool_name: &str,
    arguments: &str,
    state: SubagentActionState<'_>,
) -> Option<SubagentActionText> {
    if tool_name != SUBAGENT_TOOL_NAME {
        return None;
    }
    let outer = json_object(arguments)?;
    let request = match outer.get("request") {
        Some(Value::Object(request)) => request,
        Some(_) => return None,
        None => &outer,
    };
    let action = request.get("action").and_then(Value::as_str)?;
    let named = action == "message";
    if !named && action != "run" {
        return None;
    }
    if matches!(state, SubagentActionState::Feedback(_)) && !named {
        return None;
    }
    let raw_name = if named {
        request.get("agent").and_then(Value::as_str)?
    } else {
        "Subagent"
    };
    let raw_preview = request
        .get(if named { "message" } else { "task" })
        .and_then(Value::as_str)?;
    let name = encode_terminal_safe(raw_name.as_bytes(), SUBAGENT_NAME_BYTES).text;
    let preview = subagent_preview(raw_preview);
    let label = match state {
        SubagentActionState::Identity => name,
        SubagentActionState::Active => format!("{name} working"),
        SubagentActionState::Pending => format!("{name} still running"),
        SubagentActionState::Completed => {
            format!("{name} {}", if named { "replied" } else { "finished" })
        }
        SubagentActionState::Feedback(delivery) => format!(
            "{name} feedback {}",
            match delivery {
                SteeringDelivery::Queued => "queued",
                SteeringDelivery::Applied => "applied",
                SteeringDelivery::NotApplied => "not applied",
            }
        ),
        SubagentActionState::Stopped("Failed") => format!("{name} failed"),
        SubagentActionState::Stopped("Busy") => format!("{name} busy; message not sent"),
        SubagentActionState::Stopped("Cancelled" | "Interrupted") => {
            format!("{name} interrupted")
        }
        SubagentActionState::Stopped(reason) => format!("{reason} {name}"),
    };
    let detail = if preview.is_empty() {
        String::new()
    } else {
        format!("· {preview}")
    };
    Some(SubagentActionText { label, detail })
}

pub fn format_subagent_plain_action(
    tool_name: &str,
    arguments: &str,
    state: SubagentActionState<'_>,
) -> Option<String> {
    let action = subagent_action(tool_name, arguments, state)?;
    Some(if action.detail.is_empty() {
        action.label
    } else {
        format!("{} {}", action.label, action.detail)
    })
}

pub fn subagent_failure_label(tool_name: &str, output: &str) -> &'static str {
    if tool_name != SUBAGENT_TOOL_NAME {
        return "Failed";
    }
    let Some(result) = json_object(output) else {
        return "Failed";
    };
    if result.get("ok").and_then(Value::as_bool) != Some(false) {
        return "Failed";
    }
    match result.get("error_code").and_then(Value::as_str) {
        Some("child_busy") => "Busy",
        Some(code) if NOT_SENT_CODES.contains(&code) => "Message not sent to",
        Some(code) if INTERRUPTED_CODES.contains(&code) => "Interrupted",
        _ => "Failed",
    }
}

fn json_object(text: &str) -> Option<Map<String, Value>> {
    parse_tool_args_object(text).ok()?;
    match serde_json::from_str(text).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

fn subagent_preview(raw: &str) -> String {
    let mut end = raw.len().min(SUBAGENT_PREVIEW_SCAN_BYTES);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    let mut buffer = Vec::with_capacity(SUBAGENT_PREVIEW_BUFFER_BYTES);
    let mut pending_space = false;
    for &byte in &raw.as_bytes()[..end] {
        if is_posix_space(byte) {
            pending_space = !buffer.is_empty();
            continue;
        }
        if pending_space {
            buffer.push(b' ');
            pending_space = false;
        }
        if buffer.len() == SUBAGENT_PREVIEW_BUFFER_BYTES {
            break;
        }
        buffer.push(byte);
        if buffer.len() == SUBAGENT_PREVIEW_BUFFER_BYTES {
            break;
        }
    }
    encode_terminal_safe(&buffer, SUBAGENT_PREVIEW_BYTES).text
}

#[cfg(test)]
mod tests {
    use ofx_text::is_terminal_safe;

    use super::*;
    use crate::tool_dispatch::ToolActivity;

    const READ: CallPresentation = CallPresentation {
        activity: ToolActivity::Read,
        action_label: "Reading",
        completed_label: "Read",
        label_argument: "path",
        label_default: "file",
    };

    #[test]
    fn tool_presentation_preserves_plain_action_fallbacks() {
        let cases = [
            (r#"{"path":"src/main.zig"}"#, "Reading src/main.zig"),
            (r#"{"path":1}"#, "Reading file"),
            ("{}", "Reading file"),
            (r#"{"path":""}"#, "Reading "),
            (r#"{"path":" a "}"#, "Reading  a "),
            ("[]", "Working: read_file"),
            ("{", "Working: read_file"),
            (r#"{"path":"a.txt","line_count":1e400}"#, "Reading a.txt"),
            (r#"{"path":"a.txt","path":"b.txt"}"#, "Working: read_file"),
        ];
        for (arguments, expected) in cases {
            let label = plain_action_label(&READ, arguments);
            assert_eq!(
                format_plain_action("read_file", label.as_ref()),
                expected,
                "{arguments}"
            );
        }
    }

    fn action(arguments: &str, state: SubagentActionState<'_>) -> Option<SubagentActionText> {
        subagent_action("subagent", arguments, state)
    }

    #[test]
    fn subagent_rows_project_request_identity_state_and_bounded_safe_previews() {
        let cases = [
            (
                r#"{"request":{"action":"run","task":" Check\n cancellation\tcleanup "}}"#,
                SubagentActionState::Active,
                "Subagent working",
                "· Check cancellation cleanup",
            ),
            (
                r#"{"action":"run","task":"Check cleanup"}"#,
                SubagentActionState::Completed,
                "Subagent finished",
                "· Check cleanup",
            ),
            (
                r#"{"request":{"action":"message","agent":"reviewer","message":"Check replay","instructions":"Never display this"}}"#,
                SubagentActionState::Completed,
                "reviewer replied",
                "· Check replay",
            ),
            (
                r#"{"action":"message","agent":"reviewer","message":"Check again"}"#,
                SubagentActionState::Stopped("Failed"),
                "reviewer failed",
                "· Check again",
            ),
            (
                r#"{"action":"message","agent":"reviewer","message":"Check again"}"#,
                SubagentActionState::Stopped("Cancelled"),
                "reviewer interrupted",
                "· Check again",
            ),
            (
                r#"{"action":"run","task":"Check again"}"#,
                SubagentActionState::Stopped("Denied"),
                "Denied Subagent",
                "· Check again",
            ),
        ];
        for (arguments, state, label, detail) in cases {
            let projected = action(arguments, state).unwrap();
            assert_eq!(projected.label, label, "{arguments}");
            assert_eq!(projected.detail, detail, "{arguments}");
        }
        for arguments in [
            "{",
            "[]",
            r#"{"request":null}"#,
            r#"{"action":"inspect"}"#,
            r#"{"action":"message","message":"hello"}"#,
        ] {
            assert_eq!(
                action(arguments, SubagentActionState::Active),
                None,
                "{arguments}"
            );
        }
        let unsafe_text = action(
            r#"{"action":"message","agent":"a\u001b[2J","message":"Check 日本語\u001b[31m"}"#,
            SubagentActionState::Active,
        )
        .unwrap();
        assert!(is_terminal_safe(unsafe_text.label.as_bytes()));
        assert!(is_terminal_safe(unsafe_text.detail.as_bytes()));
        assert!(unsafe_text.detail.contains("日本語"));
        let long = subagent_preview(&"日本語".repeat(100));
        assert!(long.len() <= 120);
        assert!(is_terminal_safe(long.as_bytes()));
        assert!(long.ends_with("..."));
        assert_eq!(
            format_subagent_plain_action(
                "subagent",
                r#"{"action":"run","task":"   "}"#,
                SubagentActionState::Active,
            )
            .as_deref(),
            Some("Subagent working")
        );
        assert_eq!(
            action(
                r#"{"action":"run","task":"x"}"#,
                SubagentActionState::Feedback(SteeringDelivery::Queued),
            ),
            None
        );
        assert_eq!(
            subagent_action(
                "shell",
                r#"{"action":"run","task":"x"}"#,
                SubagentActionState::Active
            ),
            None
        );
    }

    #[test]
    fn subagent_pending_rows_do_not_claim_completion() {
        assert_eq!(
            format_subagent_plain_action(
                "subagent",
                r#"{"action":"run","task":"work"}"#,
                SubagentActionState::Pending,
            )
            .as_deref(),
            Some("Subagent still running · work")
        );
    }

    #[test]
    fn subagent_failure_labels_trust_structured_terminal_codes_only() {
        for code in ["child_interrupted", "child_cancelled", "child_lost"] {
            let output = format!(r#"{{"ok":false,"error_code":"{code}"}}"#);
            assert_eq!(subagent_failure_label("subagent", &output), "Interrupted");
        }
        for output in [
            "child_interrupted",
            "{",
            "<tool_result_preview>child_interrupted</tool_result_preview>",
            r#"{"ok":true,"error_code":"child_interrupted"}"#,
            r#"{"ok":false,"error_code":"child_failed"}"#,
        ] {
            assert_eq!(
                subagent_failure_label("subagent", output),
                "Failed",
                "{output}"
            );
        }
        assert_eq!(
            subagent_failure_label("shell", r#"{"ok":false,"error_code":"child_busy"}"#),
            "Failed"
        );
        assert_eq!(
            subagent_failure_label("subagent", r#"{"ok":false,"error_code":"child_busy"}"#),
            "Busy"
        );
        for code in NOT_SENT_CODES {
            let output = format!(r#"{{"ok":false,"error_code":"{code}"}}"#);
            assert_eq!(
                subagent_failure_label("subagent", &output),
                "Message not sent to"
            );
        }
    }

    #[test]
    fn subagent_receipts_describe_message_delivery_rather_than_child_completion() {
        let arguments =
            r#"{"request":{"action":"message","agent":"reviewer","message":"check this"}}"#;
        for (delivery, label) in [
            (
                SteeringDelivery::Queued,
                "reviewer feedback queued · check this",
            ),
            (
                SteeringDelivery::Applied,
                "reviewer feedback applied · check this",
            ),
            (
                SteeringDelivery::NotApplied,
                "reviewer feedback not applied · check this",
            ),
        ] {
            assert_eq!(
                format_subagent_plain_action(
                    "subagent",
                    arguments,
                    SubagentActionState::Feedback(delivery)
                )
                .as_deref(),
                Some(label)
            );
        }
        assert_eq!(
            format_subagent_plain_action(
                "subagent",
                arguments,
                SubagentActionState::Stopped("Busy")
            )
            .as_deref(),
            Some("reviewer busy; message not sent · check this")
        );
    }

    #[test]
    fn plain_action_labels_carry_both_tenses_and_the_target() {
        assert_eq!(
            plain_action_label(&READ, r#"{"path":"src/main.zig"}"#),
            Some(ActionLabel {
                active: "Reading",
                completed: "Read",
                target: "src/main.zig".to_owned(),
            })
        );
        assert_eq!(plain_action_label(&READ, "[]"), None);
    }
}
