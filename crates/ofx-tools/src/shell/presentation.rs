use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_contract::{ActionLabel, parse_json_value, parse_tool_args_object};
use ofx_exec::ManagedExecutions;
use ofx_text::encode_terminal_safe;
use serde_json::{Map, Value};

const MAX_RUN_COMMAND_ACTIVITY_BYTES: usize = 120;
const MAX_RUN_COMMAND_ACTIVITY_SOURCE_BYTES: usize =
    MAX_RUN_COMMAND_ACTIVITY_BYTES * MAX_RUN_COMMAND_ACTIVITY_BYTES;
const MAX_RUN_COMMAND_REFLOW_BYTES: usize = MAX_RUN_COMMAND_ACTIVITY_SOURCE_BYTES - 1;
const NOOP_DIRECTORY_PREFIX: &[u8] = b"cd . &&";
const SESSION_PREFIX: &str = "session ";

pub(super) struct ShellPresentation {
    pub(super) title: String,
    pub(super) label: Option<ActionLabel>,
}

pub(super) fn presentation(
    arguments: &str,
    workspace_root: &Path,
    executions: &ManagedExecutions,
) -> ShellPresentation {
    let (Ok(_), Some(Value::Object(fields))) = (
        parse_tool_args_object(arguments),
        parse_json_value(arguments),
    ) else {
        return ShellPresentation {
            title: "Working: shell".to_owned(),
            label: None,
        };
    };
    let root = workspace_root.as_os_str().as_bytes();
    let target = |max_bytes| session_target(&fields, root, executions, max_bytes);
    let (active, completed, title_target, label_target) = match fields.get("action") {
        Some(Value::String(action)) => match action.as_str() {
            "run" => match fields.get("command").and_then(Value::as_str) {
                Some(command) => (
                    "Running",
                    "Ran",
                    display_command(command.as_bytes(), root, MAX_RUN_COMMAND_ACTIVITY_BYTES),
                    display_command(command.as_bytes(), root, MAX_RUN_COMMAND_REFLOW_BYTES),
                ),
                None => fixed("Running", "Ran", "command"),
            },
            "interact" => match fields.get("chars").and_then(Value::as_str) {
                Some(chars) if !chars.is_empty() => (
                    "Sending input to",
                    "Sent input to",
                    target(MAX_RUN_COMMAND_ACTIVITY_BYTES),
                    target(MAX_RUN_COMMAND_REFLOW_BYTES),
                ),
                _ => (
                    "Waiting for",
                    "Observed",
                    target(MAX_RUN_COMMAND_ACTIVITY_BYTES),
                    target(MAX_RUN_COMMAND_REFLOW_BYTES),
                ),
            },
            "stop" => (
                "Stopping",
                "Stopped",
                target(MAX_RUN_COMMAND_ACTIVITY_BYTES),
                target(MAX_RUN_COMMAND_REFLOW_BYTES),
            ),
            other => fixed("Running", "Ran", other),
        },
        _ => fixed("Running", "Ran", "shell request"),
    };
    ShellPresentation {
        title: format!("{active} {title_target}"),
        label: Some(ActionLabel {
            active,
            completed,
            target: label_target,
        }),
    }
}

fn fixed(
    active: &'static str,
    completed: &'static str,
    target: &str,
) -> (&'static str, &'static str, String, String) {
    (active, completed, target.to_owned(), target.to_owned())
}

fn session_target(
    fields: &Map<String, Value>,
    root: &[u8],
    executions: &ManagedExecutions,
    max_bytes: usize,
) -> String {
    let Some(session_id) = fields.get("session_id").and_then(Value::as_str) else {
        return "shell execution".to_owned();
    };
    match executions.command(session_id) {
        Some(command) if !command.is_empty() => {
            display_command(command.as_bytes(), root, max_bytes)
        }
        _ => format!(
            "{SESSION_PREFIX}{}",
            encode_terminal_safe(
                session_id.as_bytes(),
                MAX_RUN_COMMAND_ACTIVITY_BYTES - SESSION_PREFIX.len()
            )
            .text
        ),
    }
}

fn display_command(command: &[u8], workspace_root: &[u8], max_bytes: usize) -> String {
    let projected = project_run_command(command, workspace_root, max_bytes + 1);
    encode_terminal_safe(&projected, max_bytes).text
}

fn project_run_command(command: &[u8], workspace_root: &[u8], capacity: usize) -> Vec<u8> {
    let command = strip_noop_directory_prefix(command);
    let mut root_end = workspace_root.len();
    while root_end > 1 && workspace_root[root_end - 1] == b'/' {
        root_end -= 1;
    }
    let root = &workspace_root[..root_end];
    let abbreviates = root.len() > 1 && root[0] == b'/';
    let limit = command.len().min(MAX_RUN_COMMAND_ACTIVITY_SOURCE_BYTES);
    let mut projected = Vec::with_capacity(capacity);
    let mut line_boundary_pending = false;
    let mut index = 0;
    while index < limit {
        let byte = command[index];
        if byte == b'\r' || byte == b'\n' {
            line_boundary_pending = true;
            index += 1;
            continue;
        }
        if line_boundary_pending && (byte == b' ' || byte == b'\t') {
            index += 1;
            continue;
        }
        if line_boundary_pending {
            line_boundary_pending = false;
            if !projected.is_empty() {
                projected.push(b' ');
                if projected.len() == capacity {
                    break;
                }
            }
        }
        if abbreviates && root.len() <= limit - index && root_matches_at(command, root, index) {
            projected.push(b'.');
            if projected.len() == capacity {
                break;
            }
            index += root.len();
            continue;
        }
        projected.push(byte);
        if projected.len() == capacity {
            break;
        }
        index += 1;
    }
    projected
}

fn strip_noop_directory_prefix(command: &[u8]) -> &[u8] {
    let Some(rest) = command.strip_prefix(NOOP_DIRECTORY_PREFIX) else {
        return command;
    };
    if !rest.first().is_some_and(u8::is_ascii_whitespace) && rest.first() != Some(&0x0b) {
        return command;
    }
    let start = rest
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(rest.len());
    if start == rest.len() {
        command
    } else {
        &rest[start..]
    }
}

fn root_matches_at(command: &[u8], root: &[u8], index: usize) -> bool {
    if !command[index..].starts_with(root) {
        return false;
    }
    if index > 0 && is_path_token_byte(command[index - 1]) {
        return false;
    }
    let next = index + root.len();
    next == command.len() || command[next] == b'/' || !is_path_token_byte(command[next])
}

fn is_path_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~')
}

#[cfg(test)]
mod tests {
    use ofx_exec::SessionSupervisor;

    use super::*;

    fn executions() -> ManagedExecutions {
        ManagedExecutions::new(SessionSupervisor::new("/nonexistent"))
    }

    fn presented(arguments: &str) -> ShellPresentation {
        presentation(arguments, Path::new("/work/space/"), &executions())
    }

    fn titled(arguments: &str) -> String {
        presented(arguments).title
    }

    fn labelled(arguments: &str) -> Option<(&'static str, &'static str, String)> {
        presented(arguments)
            .label
            .map(|label| (label.active, label.completed, label.target))
    }

    #[test]
    fn run_titles_show_the_command_with_the_workspace_abbreviated() {
        assert_eq!(
            titled(r#"{"action":"run","command":"touch \"/work/space/denied-marker.txt\""}"#),
            "Running touch \"./denied-marker.txt\""
        );
        assert_eq!(
            titled(r#"{"action":"run","command":"ls /work/space /work/spacecraft x/work/space"}"#),
            "Running ls . /work/spacecraft x/work/space"
        );
        assert_eq!(
            titled(r#"{"action":"run","command":"cd . &&  git status"}"#),
            "Running git status"
        );
        assert_eq!(
            titled(r#"{"action":"run","command":"cd . &&git status"}"#),
            "Running cd . &&git status"
        );
        assert_eq!(
            titled(r#"{"action":"run","command":"printf a\n    printf b\r\n\tprintf c"}"#),
            "Running printf a printf b printf c"
        );
        assert_eq!(
            titled(r#"{"action":"run","command":"printf '\u001b[31m'"}"#),
            "Running printf '\\x1b[31m'"
        );
    }

    #[test]
    fn long_run_titles_are_bounded() {
        let command = "x".repeat(500);
        let shown = titled(&format!(r#"{{"action":"run","command":"{command}"}}"#));
        assert_eq!(
            shown.len(),
            "Running ".len() + MAX_RUN_COMMAND_ACTIVITY_BYTES
        );
        assert!(shown.ends_with("..."));
    }

    #[test]
    fn labels_keep_the_command_at_the_reflow_bound_for_every_tense() {
        let command = "x".repeat(500);
        let arguments = format!(r#"{{"action":"run","command":"cd /work/space && {command}"}}"#);
        assert_eq!(
            labelled(&arguments),
            Some(("Running", "Ran", format!("cd . && {command}")))
        );
        assert!(titled(&arguments).ends_with("..."));
        let huge = "y".repeat(MAX_RUN_COMMAND_ACTIVITY_SOURCE_BYTES * 2);
        let (_, _, target) =
            labelled(&format!(r#"{{"action":"run","command":"{huge}"}}"#)).unwrap();
        assert_eq!(target.len(), MAX_RUN_COMMAND_REFLOW_BYTES);
        assert!(target.ends_with("..."));
        assert_eq!(
            labelled(r#"{"action":"interact","session_id":"shell-9"}"#),
            Some(("Waiting for", "Observed", "session shell-9".to_owned()))
        );
        assert_eq!(
            labelled(r#"{"action":"interact","session_id":"shell-9","chars":"y"}"#),
            Some((
                "Sending input to",
                "Sent input to",
                "session shell-9".to_owned()
            ))
        );
        assert_eq!(
            labelled(r#"{"action":"stop"}"#),
            Some(("Stopping", "Stopped", "shell execution".to_owned()))
        );
        assert_eq!(
            labelled(r#"{"action":"run"}"#),
            Some(("Running", "Ran", "command".to_owned()))
        );
        assert_eq!(labelled("[]"), None);
    }

    #[test]
    fn session_titles_name_the_session_when_its_command_is_unknown() {
        assert_eq!(
            titled(r#"{"action":"interact","session_id":"shell-9"}"#),
            "Waiting for session shell-9"
        );
        assert_eq!(
            titled(r#"{"action":"interact","session_id":"shell-9","chars":"y\n"}"#),
            "Sending input to session shell-9"
        );
        assert_eq!(
            titled(r#"{"action":"interact","session_id":"shell-9","chars":""}"#),
            "Waiting for session shell-9"
        );
        assert_eq!(
            titled(r#"{"action":"stop","session_id":"shell-9","force":true}"#),
            "Stopping session shell-9"
        );
        assert_eq!(titled(r#"{"action":"stop"}"#), "Stopping shell execution");
    }

    #[test]
    fn malformed_requests_keep_upstream_fallback_titles() {
        assert_eq!(titled(r#"{"action":"run"}"#), "Running command");
        assert_eq!(titled(r#"{"action":"explode"}"#), "Running explode");
        assert_eq!(titled(r#"{"command":"ls"}"#), "Running shell request");
        assert_eq!(titled(r#"{"action":7}"#), "Running shell request");
        assert_eq!(titled("[]"), "Working: shell");
        assert_eq!(titled("{"), "Working: shell");
        assert_eq!(
            titled(r#"{"action":"run","action":"stop"}"#),
            "Working: shell"
        );
    }
}
