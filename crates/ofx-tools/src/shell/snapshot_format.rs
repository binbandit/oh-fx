use ofx_contract::CommandProcessPresentation;
use ofx_exec::{CommandStatus, Snapshot, SnapshotState, StatusProjection};
use ofx_text::{
    HeadRounding, contains_ignore_case, encode_terminal_safe, is_model_safe_text,
    write_head_tail_bounded,
};
use serde_json::{Map, Value, json};

pub(super) const SHELL_PARSE_RETRY_GUIDANCE: &str = "The shell could not parse this command (unmatched quote or syntax error), so nothing executed. Rewrite the command with corrected quoting or escaping and submit the full corrected command instead of rerunning the same text.";
pub(super) const USAGE_ERROR_RETRY_GUIDANCE: &str = "The command exited with a usage error (missing or invalid arguments). Rebuild the command with the required arguments explicitly set, then submit the corrected command instead of rerunning it unchanged.";
const INCOMPLETE_OUTPUT_RETRY_GUIDANCE: &str = "Command output is incomplete. Inspect external state and available output before retrying; do not blindly rerun a command that may have changed state.";
const INDETERMINATE_STATUS_RETRY_GUIDANCE: &str = "Execution status is indeterminate. Inspect external state before retrying; do not blindly rerun a command that may have changed state.";
const LARGE_RESULT_THRESHOLD_BYTES: usize = 16 * 1024;
const OMITTED_OUTPUT_MARKER: &str = "\n... bytes omitted ...\n";
const SHELL_ERROR_PREFIXES: [&str; 4] = ["zsh", "bash", "sh", "dash"];
const SHELL_PARSE_ERROR_SIGNATURES: [&str; 4] = [
    "unmatched",
    "syntax error",
    "parse error",
    "unterminated quoted string",
];
const USAGE_BANNER_LINES: usize = 4;
const TIMEOUT_EXPIRED: &str = "TimeoutExpired";

pub(super) fn format_snapshot(snapshot: &Snapshot, max_tool_result_bytes: usize) -> String {
    let inline_max_bytes = max_tool_result_bytes.min(LARGE_RESULT_THRESHOLD_BYTES);
    if let Some(full) = format_model_safe(
        snapshot,
        &snapshot.output_delta,
        snapshot.output_truncated,
        inline_max_bytes,
    ) && full.len() <= inline_max_bytes
    {
        return full;
    }
    let mut minimum = 0;
    let mut maximum = snapshot.output_delta.len().min(inline_max_bytes);
    let mut best = None;
    while minimum <= maximum {
        let content_budget = minimum + (maximum - minimum) / 2;
        let projected = write_head_tail_bounded(
            &snapshot.output_delta,
            content_budget,
            OMITTED_OUTPUT_MARKER,
            HeadRounding::Up,
        );
        match format_model_safe(snapshot, &projected, true, inline_max_bytes) {
            Some(candidate) if candidate.len() <= inline_max_bytes => {
                best = Some(candidate);
                minimum = content_budget + 1;
            }
            _ if content_budget == 0 => break,
            _ => maximum = content_budget - 1,
        }
    }
    best.unwrap_or_else(|| format_raw(snapshot, "", true))
}

fn format_model_safe(
    snapshot: &Snapshot,
    output_delta: &[u8],
    output_truncated: bool,
    max_encoded_bytes: usize,
) -> Option<String> {
    if is_model_safe_text(output_delta) {
        return Some(format_raw(
            snapshot,
            &String::from_utf8_lossy(output_delta),
            output_truncated,
        ));
    }
    let encoded = encode_terminal_safe(output_delta, max_encoded_bytes);
    (!encoded.truncated).then(|| format_raw(snapshot, &encoded.text, output_truncated))
}

fn format_raw(snapshot: &Snapshot, output_delta: &str, output_truncated: bool) -> String {
    let projection = projection(snapshot.state);
    let retry_guidance = if snapshot.output_incomplete {
        Some(INCOMPLETE_OUTPUT_RETRY_GUIDANCE)
    } else {
        match snapshot.state {
            SnapshotState::Lost => Some(INDETERMINATE_STATUS_RETRY_GUIDANCE),
            SnapshotState::Completed(_) => {
                failure_retry_guidance(projection.exit_code, &snapshot.output_delta)
            }
            SnapshotState::Running | SnapshotState::Stopped(_) => None,
        }
    };
    let mut object = Map::new();
    object.insert(
        "session_id".to_owned(),
        json!(snapshot.retained.then_some(snapshot.execution_id.as_str())),
    );
    object.insert("state".to_owned(), json!(state_name(snapshot.state)));
    object.insert("backend".to_owned(), json!("captured"));
    object.insert("persistence".to_owned(), json!("process"));
    object.insert("output_truncated".to_owned(), json!(output_truncated));
    object.insert(
        "output_incomplete".to_owned(),
        json!(snapshot.output_incomplete),
    );
    object.insert("output_terminal_safe".to_owned(), json!(true));
    object.insert("full_output_handle".to_owned(), Value::Null);
    object.insert("exit_code".to_owned(), json!(projection.exit_code));
    object.insert("signal".to_owned(), json!(projection.signal));
    object.insert(
        "termination_indeterminate".to_owned(),
        json!(projection.termination_indeterminate),
    );
    object.insert("duration_ms".to_owned(), json!(snapshot.duration_ms));
    object.insert("accepted_bytes".to_owned(), Value::Null);
    object.insert("error".to_owned(), json!(snapshot.error_name));
    object.insert("retry_guidance".to_owned(), json!(retry_guidance));
    object.insert("output_delta".to_owned(), json!(output_delta));
    Value::Object(object).to_string()
}

pub(super) fn command_result(snapshot: &Snapshot) -> Option<String> {
    if snapshot.state == SnapshotState::Running {
        return None;
    }
    let projection = projection(snapshot.state);
    let mut object = Map::new();
    object.insert("kind".to_owned(), json!("command"));
    object.insert("command".to_owned(), json!(snapshot.command));
    object.insert("cwd".to_owned(), json!(snapshot.cwd.to_string_lossy()));
    object.insert("exit_code".to_owned(), json!(projection.exit_code));
    object.insert("signal".to_owned(), json!(projection.signal));
    object.insert(
        "timed_out".to_owned(),
        json!(snapshot.error_name == Some(TIMEOUT_EXPIRED)),
    );
    if projection.termination_indeterminate {
        object.insert("termination_indeterminate".to_owned(), json!(true));
    }
    if snapshot.output_incomplete {
        object.insert("output_incomplete".to_owned(), json!(true));
    }
    object.insert("duration_ms".to_owned(), json!(snapshot.duration_ms));
    object.insert("stdout_bytes".to_owned(), json!(snapshot.stdout_bytes));
    object.insert("stderr_bytes".to_owned(), json!(snapshot.stderr_bytes));
    object.insert("truncated".to_owned(), json!(snapshot.output_truncated));
    for name in ["output_file", "stdout_file", "stderr_file"] {
        object.insert(name.to_owned(), Value::Null);
    }
    Some(Value::Object(object).to_string())
}

pub(super) fn process_presentation(snapshot: &Snapshot) -> Option<CommandProcessPresentation> {
    if snapshot.error_name == Some(TIMEOUT_EXPIRED) {
        return Some(CommandProcessPresentation::TimedOut);
    }
    let SnapshotState::Completed(status) = snapshot.state else {
        return None;
    };
    let projection = status.project();
    match (projection.signal, projection.exit_code) {
        (Some(signal), _) => Some(CommandProcessPresentation::Signal(signal)),
        (None, Some(code)) if code != 0 => Some(CommandProcessPresentation::ExitCode(code)),
        _ => None,
    }
}

pub(super) fn projection(state: SnapshotState) -> StatusProjection {
    state
        .status()
        .map(CommandStatus::project)
        .unwrap_or_default()
}

fn state_name(state: SnapshotState) -> &'static str {
    match state {
        SnapshotState::Running => "running",
        SnapshotState::Completed(_) => "completed",
        SnapshotState::Stopped(_) => "stopped",
        SnapshotState::Lost => "lost",
    }
}

pub(super) fn failure_retry_guidance(
    exit_code: Option<i64>,
    output: &[u8],
) -> Option<&'static str> {
    if exit_code? == 0 {
        return None;
    }
    if is_shell_parse_error_output(output) {
        return Some(SHELL_PARSE_RETRY_GUIDANCE);
    }
    has_usage_banner_output(output).then_some(USAGE_ERROR_RETRY_GUIDANCE)
}

fn is_shell_parse_error_output(output: &[u8]) -> bool {
    output
        .split(|byte| *byte == b'\n')
        .map(trim_line)
        .filter(|line| has_shell_error_prefix(line))
        .any(|line| {
            SHELL_PARSE_ERROR_SIGNATURES
                .iter()
                .any(|signature| contains_ignore_case(line, signature))
        })
}

fn has_shell_error_prefix(line: &[u8]) -> bool {
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return false;
    };
    let token = &line[..colon];
    if token.is_empty() || token.contains(&b' ') {
        return false;
    }
    let base = token
        .iter()
        .rposition(|byte| *byte == b'/')
        .map_or(token, |slash| &token[slash + 1..]);
    SHELL_ERROR_PREFIXES
        .iter()
        .any(|name| base == name.as_bytes())
}

fn has_usage_banner_output(output: &[u8]) -> bool {
    output
        .split(|byte| *byte == b'\n')
        .map(trim_line_start)
        .filter(|line| !line.is_empty())
        .take(USAGE_BANNER_LINES)
        .any(|line| {
            line.get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"usage:"))
        })
}

fn is_line_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r')
}

fn trim_line_start(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|byte| !is_line_space(*byte))
        .unwrap_or(line.len());
    &line[start..]
}

fn trim_line(line: &[u8]) -> &[u8] {
    let line = trim_line_start(line);
    let end = line
        .iter()
        .rposition(|byte| !is_line_space(*byte))
        .map_or(0, |last| last + 1);
    &line[..end]
}

pub(super) fn snapshot_failed(snapshot: &Snapshot) -> bool {
    if snapshot.output_incomplete {
        return true;
    }
    match snapshot.state {
        SnapshotState::Running => false,
        SnapshotState::Completed(CommandStatus::ExitCode(code)) => code != 0,
        SnapshotState::Completed(_) | SnapshotState::Stopped(_) | SnapshotState::Lost => true,
    }
}

pub(super) fn stop_result_failed(state: SnapshotState) -> bool {
    matches!(
        state,
        SnapshotState::Lost | SnapshotState::Stopped(Some(CommandStatus::Indeterminate))
    )
}

pub(super) fn runtime_failure(code: &str) -> String {
    json!({"error": {"tool": "shell", "code": code, "retryable": false}}).to_string()
}

#[cfg(test)]
mod tests;
