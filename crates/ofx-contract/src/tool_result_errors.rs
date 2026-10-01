use ofx_text::mask_secrets;
use serde_json::{Map, Value};

#[cfg(target_os = "macos")]
const FILESYSTEM_ACCESS_DENIED_SUGGESTION: &str = "Do not retry this path unchanged or propose a symlink. oh-fx permissions cannot override the operating system. If the path is in a protected folder such as Desktop, Documents, or Downloads, ask the user to grant the terminal app Files and Folders or Full Disk Access. Otherwise, ask the user to correct OS filesystem permissions or move/copy the project to an accessible location.";
#[cfg(not(target_os = "macos"))]
const FILESYSTEM_ACCESS_DENIED_SUGGESTION: &str = "Do not retry this path unchanged or propose a symlink. oh-fx permissions cannot override the operating system. Ask the user to correct OS filesystem permissions or move/copy the project to an accessible location.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionFailure<'a> {
    pub tool_name: &'a str,
    pub message: &'a str,
    pub details: &'a [(&'a str, &'a str)],
    pub suggestion: Option<&'a str>,
}

pub fn tool_execution_failure_json(failure: &ExecutionFailure<'_>) -> String {
    let mut error = Map::new();
    error.insert("type".to_owned(), Value::from("tool_execution_failed"));
    error.insert("tool_name".to_owned(), masked(failure.tool_name));
    error.insert("message".to_owned(), masked(failure.message));
    if !failure.details.is_empty() {
        let details: Map<String, Value> = failure
            .details
            .iter()
            .map(|(name, value)| ((*name).to_owned(), masked(value)))
            .collect();
        error.insert("details".to_owned(), Value::Object(details));
    }
    if let Some(suggestion) = failure.suggestion {
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
    fn filesystem_access_denied_names_the_path_and_error() {
        let body = filesystem_access_denied_json("glob_files", "/tmp/blocked", "AccessDenied");
        assert!(body.starts_with(
            "{\"error\":{\"type\":\"tool_execution_failed\",\"tool_name\":\"glob_files\",\"message\":\"Operating system denied filesystem access\",\"details\":{\"path\":\"/tmp/blocked\",\"error\":\"AccessDenied\"},\"suggestion\":\"Do not retry this path unchanged or propose a symlink."
        ));
    }
}
