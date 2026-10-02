use std::fmt::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_contract::{ChatMessage, ToolCallId};
use ofx_markdown::FileReview;
use ofx_text::{HeadRounding, encode_terminal_safe, write_head_tail_bounded};

use super::ReviewSubject;

const MAX_ACTION_FIELD_BYTES: usize = 64 * 1024;
const MAX_REVIEW_EVIDENCE_BYTES: usize = MAX_ACTION_FIELD_BYTES;
const CURRENT_BRANCH_MAX_BYTES: usize = 255;
const MAX_PRIOR_TOOL_RESULT_ENTRIES: usize = 16;
const MAX_PRIOR_TOOL_RESULT_FIELD_BYTES: usize = 512;
const MAX_PRIOR_TOOL_RESULT_CONTENT_BYTES: usize = 1024;
const MAX_PRIOR_TOOL_RESULT_EVIDENCE_BYTES: usize = 8 * 1024;
const EVIDENCE_OMITTED_MARKER: &str = " ...[evidence omitted]... ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) role: &'static str,
    pub(crate) path: Vec<u8>,
}

pub(crate) enum Action<'a> {
    Command {
        command: &'a str,
        cwd: &'a Path,
    },
    ShellInput {
        arguments_json: &'a str,
    },
    FileMutation {
        tool_name: &'a str,
        display_path: &'a str,
        preimage_present: bool,
        review: FileReview<'a>,
    },
    Tool {
        tool_name: &'a str,
        arguments_json: &'a str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PriorToolResult<'a> {
    call_id: &'a str,
    tool_name: &'a str,
    content: &'a str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PriorToolResults<'a> {
    entries: Vec<PriorToolResult<'a>>,
    older_entries_omitted: bool,
}

#[cfg(test)]
impl<'a> PriorToolResults<'a> {
    pub(super) fn entries(&self) -> &[PriorToolResult<'a>] {
        &self.entries
    }
}

#[cfg(test)]
impl<'a> PriorToolResult<'a> {
    pub(super) fn call_id(&self) -> &'a str {
        self.call_id
    }

    pub(super) fn content(&self) -> &'a str {
        self.content
    }
}

pub(super) struct Evidence {
    pub(super) text: String,
    pub(super) action_complete: bool,
}

pub(crate) fn select_prior_tool_results<'a>(
    turn: &'a [ChatMessage],
    target: &ToolCallId,
    held: &[(ToolCallId, String)],
) -> PriorToolResults<'a> {
    let pending = |message: &ChatMessage| {
        matches!(message, ChatMessage::Assistant { tool_calls, .. }
            if tool_calls.iter().any(|call| call.id == *target))
    };
    let Some(boundary) = turn
        .iter()
        .rposition(pending)
        .filter(|_| !target.as_str().is_empty())
    else {
        return PriorToolResults::default();
    };
    let mut selected = PriorToolResults::default();
    for message in turn[..boundary].iter().rev() {
        let ChatMessage::Tool {
            call_id,
            tool_name,
            content,
            ..
        } = message
        else {
            continue;
        };
        if held
            .iter()
            .any(|(held_id, held_content)| held_id == call_id && held_content == content)
        {
            continue;
        }
        if selected.entries.len() == MAX_PRIOR_TOOL_RESULT_ENTRIES {
            selected.older_entries_omitted = true;
            break;
        }
        selected.entries.push(PriorToolResult {
            call_id: call_id.as_str(),
            tool_name,
            content,
        });
    }
    selected.entries.reverse();
    selected
}

pub(super) fn serialize(subject: &ReviewSubject<'_>) -> Evidence {
    let mut out = String::new();
    let mut complete = true;
    write_prior_tool_results(&mut out, &subject.prior_tool_results);
    if let Some(branch) = &subject.proven_current_branch {
        write_bounded_field(
            &mut out,
            "proven_current_branch",
            branch.as_bytes(),
            CURRENT_BRANCH_MAX_BYTES,
            &mut complete,
        );
    }
    for target in &subject.targets {
        let _ = write!(out, "target[{}]: ", target.role);
        write_bounded_value(
            &mut out,
            &target.path,
            MAX_ACTION_FIELD_BYTES,
            &mut complete,
        );
        out.push('\n');
    }
    match &subject.action {
        Action::ShellInput { arguments_json } => {
            out.push_str("action: shell_input\ntool: shell\n");
            write_action_field(&mut out, "arguments_json", arguments_json, &mut complete);
            complete = false;
            out.push_str("receiver: [evidence unavailable]\n");
        }
        Action::Command { command, cwd } => {
            out.push_str("action: command\n");
            write_action_field(&mut out, "command", command, &mut complete);
            write_bounded_field(
                &mut out,
                "cwd",
                cwd.as_os_str().as_bytes(),
                MAX_ACTION_FIELD_BYTES,
                &mut complete,
            );
            let _ = writeln!(
                out,
                "background: false\ntarget_os: {}",
                std::env::consts::OS
            );
        }
        Action::FileMutation {
            tool_name,
            display_path,
            preimage_present,
            review,
        } => {
            out.push_str("action: prepared_file_mutation\n");
            write_action_field(&mut out, "tool", tool_name, &mut complete);
            write_action_field(&mut out, "path", display_path, &mut complete);
            let preimage = if *preimage_present {
                "present"
            } else {
                "absent"
            };
            let _ = writeln!(
                out,
                "preimage: {preimage}\nadditions: {}\ndeletions: {}",
                review.additions(),
                review.deletions()
            );
            write_review_rows(&mut out, review, &mut complete);
        }
        Action::Tool {
            tool_name,
            arguments_json,
        } => {
            out.push_str("action: tool\n");
            write_action_field(&mut out, "tool", tool_name, &mut complete);
            write_action_field(&mut out, "arguments_json", arguments_json, &mut complete);
        }
    }
    let _ = writeln!(out, "action_evidence_incomplete: {}", !complete);
    Evidence {
        text: out,
        action_complete: complete,
    }
}

fn write_review_rows(out: &mut String, review: &FileReview<'_>, complete: &mut bool) {
    let review_start = out.len();
    let mut rows = review.rows();
    let overflowed = rows.by_ref().any(|line| {
        let _ = write!(out, "review[{}]: ", line.op.as_str());
        write_bounded_value(out, line.text, MAX_REVIEW_EVIDENCE_BYTES, complete);
        out.push('\n');
        out.len() - review_start > MAX_REVIEW_EVIDENCE_BYTES
    });
    if overflowed {
        *complete = false;
        let _ = writeln!(out, "review_omitted_rows: {}", rows.count());
    }
}

pub(super) fn write_prior_tool_results(out: &mut String, results: &PriorToolResults<'_>) {
    let mut evidence_complete = !results.older_entries_omitted;
    let rendered: Vec<(String, bool)> = results
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| render_prior_tool_result(index, entry))
        .collect();
    if rendered.iter().any(|(_, complete)| !complete) {
        evidence_complete = false;
    }
    let mut included = vec![false; rendered.len()];
    let mut used_bytes = 0;
    for (index, (text, _)) in rendered.iter().enumerate().rev() {
        if text.len() <= MAX_PRIOR_TOOL_RESULT_EVIDENCE_BYTES.saturating_sub(used_bytes) {
            included[index] = true;
            used_bytes += text.len();
        } else {
            evidence_complete = false;
        }
    }
    let mut serialized = 0;
    for ((text, _), include) in rendered.iter().zip(&included) {
        if *include {
            out.push_str(text);
            serialized += 1;
        }
    }
    let _ = write!(
        out,
        "prior_tool_results_serialized: {serialized}\nprior_tool_results_selected_not_serialized: {}\nprior_tool_results_older_omitted: {}\nprior_tool_result_evidence_incomplete: {}\n",
        rendered.len() - serialized,
        results.older_entries_omitted,
        !evidence_complete
    );
}

fn render_prior_tool_result(index: usize, entry: &PriorToolResult<'_>) -> (String, bool) {
    let mut out = String::new();
    let mut complete = true;
    let _ = write!(out, "prior_tool_result[{index}].tool_call_id: ");
    write_bounded_value(
        &mut out,
        entry.call_id.as_bytes(),
        MAX_PRIOR_TOOL_RESULT_FIELD_BYTES,
        &mut complete,
    );
    let _ = write!(out, "\nprior_tool_result[{index}].tool: ");
    write_bounded_value(
        &mut out,
        entry.tool_name.as_bytes(),
        MAX_PRIOR_TOOL_RESULT_FIELD_BYTES,
        &mut complete,
    );
    let _ = write!(out, "\nprior_tool_result[{index}].content_untrusted: ");
    write_bounded_value(
        &mut out,
        entry.content.as_bytes(),
        MAX_PRIOR_TOOL_RESULT_CONTENT_BYTES,
        &mut complete,
    );
    out.push('\n');
    (out, complete)
}

fn write_action_field(out: &mut String, label: &str, value: &str, complete: &mut bool) {
    write_bounded_field(
        out,
        label,
        value.as_bytes(),
        MAX_ACTION_FIELD_BYTES,
        complete,
    );
}

fn write_bounded_field(
    out: &mut String,
    label: &str,
    value: &[u8],
    cap: usize,
    complete: &mut bool,
) {
    out.push_str(label);
    out.push_str(": ");
    write_bounded_value(out, value, cap, complete);
    out.push('\n');
}

fn write_bounded_value(out: &mut String, value: &[u8], cap: usize, complete: &mut bool) {
    let encoded = encode_terminal_safe(value, usize::MAX).text;
    if encoded.len() <= cap {
        out.push_str(&encoded);
        return;
    }
    *complete = false;
    let bounded = write_head_tail_bounded(
        encoded.as_bytes(),
        cap,
        EVIDENCE_OMITTED_MARKER,
        HeadRounding::Down,
    );
    out.push_str(&String::from_utf8_lossy(&bounded));
}
