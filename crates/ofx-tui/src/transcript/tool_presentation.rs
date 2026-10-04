use ofx_contract::{
    ActionLabel, CONTEXT_DEFERRED_TOOL_OUTPUT, CallDescription, CommandProcessPresentation,
    DEFERRED_TOOL_OUTPUT, FileChangeStats, ReasoningEffort, SavedToolCall, SubagentActionState,
    SubagentStatus, ToolActivity, ToolArgsError, ToolCallId, ToolDeferral,
    ToolPermissionDenialReason, ToolRejection, ToolResultStatus, ToolStatusDetail, TurnOutcome,
    format_unknown_action, is_captured_command, parse_tool_args_object,
    shell_request_invalid_field_count, subagent_action, subagent_failure_label,
    subagent_result_state, tool_permission_denial_reason,
};
use ofx_text::{encode_terminal_safe, encode_terminal_safe_inline, mask_secrets};

use crate::render::compact_model_label;

const MAX_RUN_COMMAND_ACTIVITY_BYTES: usize = 120;
const MAX_TARGET_BYTES: usize = MAX_RUN_COMMAND_ACTIVITY_BYTES * MAX_RUN_COMMAND_ACTIVITY_BYTES - 1;
const MAX_FAILURE_DETAIL_BYTES: usize = 256;
const SHELL_TOOL: &str = "shell";
const SUBAGENT_TOOL: &str = "subagent";
const SUBAGENT_ACTIVE_VERB: &str = " working";
const STATUS_SEPARATOR: &str = " · ";
const FILE_MUTATION_TOOLS: [&str; 2] = ["write_file", "edit_file"];
pub(crate) const FILE_MUTATION_TARGET: &str = "file";
const INVALID_ARGUMENTS_TARGET: &str = "tool call";
const NOOP_RESULT_PREFIX: &str = "No changes to ";
const FAILED: &str = "Failed";
const CANCELLED: &str = "Cancelled";
const RUNNING: &str = "Running";
const NOT_EXECUTED: &str = "Not executed";
const READING_INSTRUCTIONS: &str = "Reading project instructions before continuing:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolOutcome {
    Completed,
    Failed,
    Denied,
    Cancelled,
    Deferred,
    Unreported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolStatus {
    pub(crate) outcome: Option<ToolOutcome>,
    pub(crate) phrase: String,
    pub(crate) label_len: usize,
    pub(crate) process: Option<CommandProcessPresentation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolActivityRow {
    pub(crate) call_id: ToolCallId,
    tool_name: String,
    pub(crate) activity: Option<ToolActivity>,
    label: Option<ActionLabel>,
    title: String,
    pub(crate) status: ToolStatus,
    child_status: Option<String>,
}

pub(crate) struct Finished<'a> {
    pub(crate) arguments: &'a str,
    pub(crate) status: ToolResultStatus,
    pub(crate) content: &'a str,
    pub(crate) process: Option<CommandProcessPresentation>,
    pub(crate) status_detail: Option<ToolStatusDetail>,
    pub(crate) file_change: Option<FileChangeStats>,
}

pub(crate) struct Rejected<'a> {
    pub(crate) reason: ToolRejection,
    pub(crate) arguments: &'a str,
    pub(crate) description: Option<CallDescription>,
    pub(crate) content: &'a str,
}

impl ToolActivityRow {
    pub(crate) fn started(
        call_id: ToolCallId,
        tool_name: &str,
        description: CallDescription,
    ) -> Self {
        let mut row = Self::new(call_id, tool_name, Some(description));
        row.status = row.active_status();
        row
    }

    fn new(call_id: ToolCallId, tool_name: &str, description: Option<CallDescription>) -> Self {
        let (activity, label, title) = match description {
            Some(description) => (
                Some(description.activity),
                description.label.map(|label| ActionLabel {
                    target: encoded_target(&label.target),
                    ..label
                }),
                encoded_target(&description.title),
            ),
            None => (None, None, String::new()),
        };
        Self {
            call_id,
            tool_name: encoded_target(tool_name),
            activity,
            label,
            title,
            status: ToolStatus {
                outcome: None,
                phrase: String::new(),
                label_len: 0,
                process: None,
            },
            child_status: None,
        }
    }

    pub(crate) fn saved(call: SavedToolCall) -> Self {
        let mut row = Self::new(call.call_id, &call.tool_name, call.description);
        if row.title.is_empty() {
            row.title = encoded_target(&format_unknown_action(&call.tool_name));
        }
        let (outcome, label) = row.saved_outcome(call.status, &call.output);
        let command = call
            .process
            .filter(|_| matches!(outcome, ToolOutcome::Completed | ToolOutcome::Failed))
            .filter(|_| is_captured_command(&call.tool_name, &call.arguments));
        row.status = match command {
            Some(process) => {
                let (outcome, label) = command_outcome(process);
                row.settled(outcome, &label, None, Some(process))
            }
            None => row
                .saved_subagent_status(&call.arguments, &call.output, outcome, label)
                .unwrap_or_else(|| row.settled(outcome, label, None, None)),
        };
        row
    }

    fn saved_subagent_status(
        &self,
        arguments: &str,
        output: &str,
        outcome: ToolOutcome,
        label: &'static str,
    ) -> Option<ToolStatus> {
        if !self.delegates() {
            return None;
        }
        let state = match outcome {
            ToolOutcome::Denied | ToolOutcome::Deferred => SubagentActionState::Stopped(label),
            _ => subagent_result_state(SUBAGENT_TOOL, output).unwrap_or(
                if outcome == ToolOutcome::Completed {
                    SubagentActionState::Completed
                } else {
                    SubagentActionState::Stopped(subagent_failure_label(SUBAGENT_TOOL, output))
                },
            ),
        };
        subagent_status(outcome, arguments, state)
    }

    fn saved_outcome(&self, status: ToolResultStatus, output: &str) -> (ToolOutcome, &'static str) {
        let failed = status == ToolResultStatus::Failure;
        if failed && output == CONTEXT_DEFERRED_TOOL_OUTPUT {
            (ToolOutcome::Deferred, READING_INSTRUCTIONS)
        } else if failed && output == DEFERRED_TOOL_OUTPUT {
            (ToolOutcome::Denied, NOT_EXECUTED)
        } else if let Some(reason) = tool_permission_denial_reason(output) {
            (ToolOutcome::Denied, denial_label(reason))
        } else if failed {
            (ToolOutcome::Failed, FAILED)
        } else {
            let completed = self
                .label
                .as_ref()
                .map_or("Completed", |label| label.completed);
            (ToolOutcome::Completed, completed)
        }
    }

    pub(crate) fn rejected(call_id: ToolCallId, tool_name: &str, rejected: Rejected<'_>) -> Self {
        let known = rejected.description.is_some()
            || !matches!(
                rejected.reason,
                ToolRejection::Unsupported | ToolRejection::Panicked
            );
        let mut row = Self::new(call_id, tool_name, rejected.description);
        let suffix = match rejected.reason {
            ToolRejection::MalformedArguments => Some(malformed_suffix(rejected.arguments)),
            ToolRejection::Invalid => row.correction_suffix(rejected.content),
            ToolRejection::Unsupported | ToolRejection::Panicked => None,
        };
        row.status = if known {
            row.settled(ToolOutcome::Failed, FAILED, suffix.as_deref(), None)
        } else {
            let phrase = format!("{FAILED} {}", row.tool_name);
            ToolStatus {
                outcome: Some(ToolOutcome::Failed),
                phrase,
                label_len: FAILED.len(),
                process: None,
            }
        };
        row
    }

    pub(crate) fn is_active(&self) -> bool {
        self.status.outcome.is_none()
    }

    pub(crate) fn finish(&mut self, finished: &Finished<'_>) {
        let denial = (finished.status == ToolResultStatus::Failure)
            .then(|| tool_permission_denial_reason(finished.content))
            .flatten();
        if let Some(status) = self.subagent_outcome(finished, denial) {
            self.status = status;
            return;
        }
        self.status = if let Some(reason) = denial {
            self.settled(ToolOutcome::Denied, denial_label(reason), None, None)
        } else if let Some((outcome, label)) = self.process_outcome(finished.process) {
            self.settled(outcome, &label, None, finished.process)
        } else if finished.status == ToolResultStatus::Success {
            if self.is_file_mutation() && finished.content.starts_with(NOOP_RESULT_PREFIX) {
                ToolStatus {
                    outcome: Some(ToolOutcome::Completed),
                    phrase: encoded_target(finished.content),
                    label_len: 0,
                    process: None,
                }
            } else {
                let completed = self
                    .label
                    .as_ref()
                    .map_or("Completed", |label| label.completed);
                let stats = finished.file_change.and_then(stats_suffix);
                self.settled(ToolOutcome::Completed, completed, stats.as_deref(), None)
            }
        } else {
            let suffix = self.correction_suffix(finished.content).or_else(|| {
                failure_detail(&self.tool_name, finished.status_detail, finished.content)
            });
            self.settled(ToolOutcome::Failed, FAILED, suffix.as_deref(), None)
        };
    }

    pub(crate) fn defer(&mut self, deferral: ToolDeferral) {
        if self.is_active() {
            self.status = match deferral {
                ToolDeferral::ProjectInstructions => {
                    self.settled(ToolOutcome::Deferred, READING_INSTRUCTIONS, None, None)
                }
                ToolDeferral::TargetChanged => {
                    self.settled(ToolOutcome::Denied, NOT_EXECUTED, None, None)
                }
            };
        }
    }

    pub(crate) fn cancel(&mut self) {
        if self.is_active() {
            self.status = self.settled(ToolOutcome::Cancelled, CANCELLED, None, None);
            self.child_status = None;
        }
    }

    pub(crate) fn report_child(&mut self, status: &SubagentStatus) {
        if !self.delegates() {
            return;
        }
        let mut line = compact_model_label(&status.model);
        if let ReasoningEffort::Named(effort) = &status.effort {
            line.push_str(STATUS_SEPARATOR);
            line.push_str(effort);
        }
        self.child_status = Some(encoded_target(&line));
    }

    pub(crate) fn child_status(&self) -> Option<&str> {
        self.child_status.as_deref()
    }

    pub(crate) fn abandon(&mut self, outcome: TurnOutcome) {
        if !self.is_active() {
            return;
        }
        self.child_status = None;
        let (outcome, phrase) = match outcome {
            TurnOutcome::Completed => (ToolOutcome::Unreported, "Tool completion was not reported"),
            TurnOutcome::Interrupted => (ToolOutcome::Cancelled, "Tool cancelled"),
            TurnOutcome::Failed => (ToolOutcome::Failed, "Tool failed"),
        };
        self.status = ToolStatus {
            outcome: Some(outcome),
            phrase: phrase.to_owned(),
            label_len: phrase.len(),
            process: None,
        };
    }

    pub(crate) fn shows_diff_stats(&self) -> bool {
        self.is_file_mutation()
    }

    pub(crate) fn command_display(&self) -> Option<(&'static str, String)> {
        let label = self.label.as_ref()?;
        (self.activity == Some(ToolActivity::Command))
            .then(|| (label.completed, label.target.clone()))
    }

    pub(crate) fn cancellation_target(&self) -> Option<(String, bool)> {
        let phrase = &self.status.phrase;
        if self.status.label_len >= phrase.len() {
            return None;
        }
        Some(match &self.label {
            Some(label) => (
                self.bounded_target(label),
                self.tool_name == SHELL_TOOL && label.active == RUNNING,
            ),
            None => (phrase[self.status.label_len + 1..].to_owned(), false),
        })
    }

    fn is_file_mutation(&self) -> bool {
        FILE_MUTATION_TOOLS.contains(&self.tool_name.as_str())
    }

    fn delegates(&self) -> bool {
        self.tool_name == SUBAGENT_TOOL && self.activity == Some(ToolActivity::Subagent)
    }

    fn subagent_outcome(
        &self,
        finished: &Finished<'_>,
        denial: Option<ToolPermissionDenialReason>,
    ) -> Option<ToolStatus> {
        if !self.delegates() {
            return None;
        }
        let (outcome, state) = match denial {
            Some(reason) => (
                ToolOutcome::Denied,
                SubagentActionState::Stopped(denial_label(reason)),
            ),
            None if finished.status == ToolResultStatus::Success => {
                (ToolOutcome::Completed, SubagentActionState::Completed)
            }
            None => (
                ToolOutcome::Failed,
                SubagentActionState::Stopped(subagent_failure_label(
                    SUBAGENT_TOOL,
                    finished.content,
                )),
            ),
        };
        subagent_status(outcome, finished.arguments, state)
    }

    fn subagent_identity(&self) -> Option<String> {
        let (name, rest) = self
            .delegates()
            .then(|| self.title.split_once(SUBAGENT_ACTIVE_VERB))
            .flatten()?;
        (!name.is_empty() && !name.contains(' ')).then(|| format!("{name}{rest}"))
    }

    fn active_status(&self) -> ToolStatus {
        let (phrase, label_len) = if let Some(label) = &self.label {
            (
                format!("{} {}", label.active, label.target),
                label.active.len(),
            )
        } else {
            (self.title.clone(), self.title.len())
        };
        ToolStatus {
            outcome: None,
            phrase,
            label_len,
            process: None,
        }
    }

    fn settled(
        &self,
        outcome: ToolOutcome,
        label: &str,
        suffix: Option<&str>,
        process: Option<CommandProcessPresentation>,
    ) -> ToolStatus {
        let target = match (&self.label, suffix) {
            (Some(action), Some(_)) => self.bounded_target(action),
            (Some(action), None) => action.target.clone(),
            (None, _) if self.is_file_mutation() => FILE_MUTATION_TARGET.to_owned(),
            (None, _) => self
                .subagent_identity()
                .unwrap_or_else(|| INVALID_ARGUMENTS_TARGET.to_owned()),
        };
        ToolStatus {
            outcome: Some(outcome),
            phrase: format!("{label} {target}{}", suffix.unwrap_or_default()),
            label_len: label.len(),
            process,
        }
    }

    fn bounded_target(&self, action: &ActionLabel) -> String {
        let bounded = self
            .title
            .strip_prefix(action.active)
            .and_then(|rest| rest.strip_prefix(' '))
            .filter(|_| self.activity == Some(ToolActivity::Command));
        bounded.unwrap_or(&action.target).to_owned()
    }

    fn process_outcome(
        &self,
        process: Option<CommandProcessPresentation>,
    ) -> Option<(ToolOutcome, String)> {
        if self.activity != Some(ToolActivity::Command) {
            return None;
        }
        process.map(command_outcome)
    }

    fn correction_suffix(&self, content: &str) -> Option<String> {
        if self.tool_name != SHELL_TOOL {
            return None;
        }
        let count = shell_request_invalid_field_count(content)?;
        Some(format!(
            " · {count} invalid field{}",
            if count == 1 { "" } else { "s" }
        ))
    }
}

fn subagent_status(
    outcome: ToolOutcome,
    arguments: &str,
    state: SubagentActionState<'_>,
) -> Option<ToolStatus> {
    let action = subagent_action(SUBAGENT_TOOL, arguments, state)?;
    let phrase = if action.detail.is_empty() {
        action.label.clone()
    } else {
        format!("{} {}", action.label, action.detail)
    };
    Some(ToolStatus {
        outcome: Some(outcome),
        phrase,
        label_len: action.label.len(),
        process: None,
    })
}

fn command_outcome(process: CommandProcessPresentation) -> (ToolOutcome, String) {
    match process {
        CommandProcessPresentation::ExitCode(0) => (ToolOutcome::Completed, "Ran".to_owned()),
        CommandProcessPresentation::ExitCode(code) => {
            (ToolOutcome::Failed, format!("Exited {code}"))
        }
        CommandProcessPresentation::Signal(signal) => {
            (ToolOutcome::Failed, format!("Signaled {signal}"))
        }
        CommandProcessPresentation::TimedOut => (ToolOutcome::Failed, "Timed out".to_owned()),
        CommandProcessPresentation::OutputCaptureFailed => {
            (ToolOutcome::Failed, "Output capture failed".to_owned())
        }
    }
}

fn denial_label(reason: ToolPermissionDenialReason) -> &'static str {
    match reason {
        ToolPermissionDenialReason::UserDenied | ToolPermissionDenialReason::PolicyDenied => {
            "Denied"
        }
        ToolPermissionDenialReason::AutoDenied => "Denied by auto agent",
        ToolPermissionDenialReason::ReviewCaution => "Safety caution",
        ToolPermissionDenialReason::ReviewEvidenceIncomplete => "Review evidence incomplete",
        ToolPermissionDenialReason::ReviewUnavailable => "Review unavailable",
        ToolPermissionDenialReason::PermissionRequired => "Permission required",
    }
}

fn stats_suffix(change: FileChangeStats) -> Option<String> {
    match (change.additions, change.deletions) {
        (0, 0) => None,
        (additions, 0) => Some(format!(" +{additions}")),
        (0, deletions) => Some(format!(" -{deletions}")),
        (additions, deletions) => Some(format!(" +{additions} / -{deletions}")),
    }
}

fn malformed_suffix(arguments: &str) -> String {
    let detail = match parse_tool_args_object(arguments) {
        Err(ToolArgsError::NotObject) => "non-object arguments",
        Ok(_) | Err(ToolArgsError::InvalidJson) => "invalid JSON arguments",
    };
    format!(": {detail}")
}

fn failure_detail(
    tool_name: &str,
    status_detail: Option<ToolStatusDetail>,
    content: &str,
) -> Option<String> {
    let detail = status_detail?;
    if detail != ToolStatusDetail::PreflightFailed {
        return Some(format!(": {}", detail.text()));
    }
    let actionable = FILE_MUTATION_TOOLS
        .contains(&tool_name)
        .then(|| content.strip_prefix(tool_name)?.strip_prefix(" failed: "))
        .flatten()
        .filter(|actionable| !actionable.is_empty() && !actionable.contains(['\n', '\r']));
    let source = actionable.unwrap_or(content);
    if source.is_empty() {
        return Some(format!(": {}", detail.text()));
    }
    let encoded =
        encode_terminal_safe_inline(mask_secrets(source).as_bytes(), MAX_FAILURE_DETAIL_BYTES);
    Some(format!(
        ": {}",
        if encoded.text.is_empty() {
            detail.text()
        } else {
            &encoded.text
        }
    ))
}

fn encoded_target(raw: &str) -> String {
    encode_terminal_safe(raw.as_bytes(), MAX_TARGET_BYTES).text
}

#[cfg(test)]
mod tests {
    use ofx_contract::{
        Concurrency, ReasoningEffort, ReviewFailure, ReviewHold, SubagentActionState,
        SubagentStatus, ToolEffect, format_subagent_plain_action, tool_permission_denied_json,
        tool_review_held_json,
    };

    use super::*;

    fn description(
        activity: ToolActivity,
        label: Option<(&'static str, &'static str, &str)>,
        title: &str,
    ) -> CallDescription {
        CallDescription {
            title: title.to_owned(),
            label: label.map(|(active, completed, target)| ActionLabel {
                active,
                completed,
                target: target.to_owned(),
            }),
            activity,
            effect: ToolEffect::ReadOnly,
            concurrency: Concurrency::Parallel,
        }
    }

    fn row(
        tool: &str,
        activity: ToolActivity,
        label: (&'static str, &'static str, &str),
    ) -> ToolActivityRow {
        let title = format!("{} {}", label.0, label.2);
        ToolActivityRow::started(
            ToolCallId::new("call"),
            tool,
            description(activity, Some(label), &title),
        )
    }

    fn finished(row: &mut ToolActivityRow, status: ToolResultStatus, content: &str) -> String {
        finish_with(row, status, content, None, None)
    }

    fn finish_with(
        row: &mut ToolActivityRow,
        status: ToolResultStatus,
        content: &str,
        process: Option<CommandProcessPresentation>,
        status_detail: Option<ToolStatusDetail>,
    ) -> String {
        row.finish(&Finished {
            arguments: "{}",
            status,
            content,
            process,
            status_detail,
            file_change: None,
        });
        row.status.phrase.clone()
    }

    const RUN_CHILD: &str = r#"{"request":{"action":"run","task":"inspect auth"}}"#;
    const MESSAGE_CHILD: &str =
        r#"{"request":{"action":"message","agent":"reviewer","message":"check this"}}"#;

    fn subagent_row(arguments: &str) -> ToolActivityRow {
        let title =
            format_subagent_plain_action("subagent", arguments, SubagentActionState::Active)
                .unwrap();
        ToolActivityRow::started(
            ToolCallId::new("call"),
            "subagent",
            CallDescription {
                title,
                label: None,
                activity: ToolActivity::Subagent,
                effect: ToolEffect::Mutating,
                concurrency: Concurrency::Parallel,
            },
        )
    }

    #[test]
    fn rows_name_the_active_and_completed_action() {
        let mut read = row(
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", "README.md"),
        );
        assert_eq!(read.status.phrase, "Reading README.md");
        assert!(read.is_active());
        assert_eq!(
            finished(&mut read, ToolResultStatus::Success, "1\talpha"),
            "Read README.md"
        );
        assert_eq!(read.status.outcome, Some(ToolOutcome::Completed));
        let mut glob = row(
            "glob_files",
            ToolActivity::List,
            ("Matching", "Matched", "*.rs"),
        );
        assert_eq!(
            finished(&mut glob, ToolResultStatus::Success, ""),
            "Matched *.rs"
        );
        let mut grep = row(
            "grep_files",
            ToolActivity::Read,
            ("Searching", "Searched", "beta"),
        );
        assert_eq!(
            finished(&mut grep, ToolResultStatus::Success, ""),
            "Searched beta"
        );
        let mut write = row(
            "write_file",
            ToolActivity::Write,
            ("Writing", "Wrote", "new.txt"),
        );
        assert_eq!(
            finished(
                &mut write,
                ToolResultStatus::Success,
                "wrote new.txt (4 bytes)"
            ),
            "Wrote new.txt"
        );
        let mut edit = row(
            "edit_file",
            ToolActivity::Edit,
            ("Editing", "Edited", "README.md"),
        );
        assert_eq!(
            finished(&mut edit, ToolResultStatus::Success, "edited"),
            "Edited README.md"
        );
    }

    #[test]
    fn completed_file_changes_count_their_lines() {
        let cases = [
            ((2, 0), "Wrote new.txt +2"),
            ((0, 3), "Wrote new.txt -3"),
            ((2, 1), "Wrote new.txt +2 / -1"),
            ((0, 0), "Wrote new.txt"),
        ];
        for ((additions, deletions), phrase) in cases {
            let mut write = row(
                "write_file",
                ToolActivity::Write,
                ("Writing", "Wrote", "new.txt"),
            );
            write.finish(&Finished {
                arguments: "{}",
                status: ToolResultStatus::Success,
                content: "wrote new.txt (4 bytes)",
                process: None,
                status_detail: None,
                file_change: Some(FileChangeStats {
                    additions,
                    deletions,
                }),
            });
            assert_eq!(write.status.phrase, phrase);
            assert_eq!(write.status.outcome, Some(ToolOutcome::Completed));
        }
    }

    #[test]
    fn unchanged_file_mutations_show_their_result() {
        let mut write = row(
            "write_file",
            ToolActivity::Write,
            ("Writing", "Wrote", "README.md"),
        );
        let content = "No changes to README.md; it already contains the requested content";
        assert_eq!(
            finished(&mut write, ToolResultStatus::Success, content),
            content
        );
        assert_eq!(write.status.outcome, Some(ToolOutcome::Completed));
        let mut read = row("read_file", ToolActivity::Read, ("Reading", "Read", "a"));
        assert_eq!(
            finished(&mut read, ToolResultStatus::Success, content),
            "Read a"
        );
    }

    #[test]
    fn denials_name_their_reason() {
        let mut shell = row(
            "shell",
            ToolActivity::Command,
            ("Running", "Ran", "touch made.txt"),
        );
        assert_eq!(
            finished(
                &mut shell,
                ToolResultStatus::Failure,
                &tool_permission_denied_json("shell")
            ),
            "Denied touch made.txt"
        );
        assert_eq!(shell.status.outcome, Some(ToolOutcome::Denied));
        let mut write = row(
            "write_file",
            ToolActivity::Write,
            ("Writing", "Wrote", "file"),
        );
        assert_eq!(
            finished(
                &mut write,
                ToolResultStatus::Failure,
                &tool_review_held_json(
                    "write_file",
                    ReviewHold::Unavailable(ReviewFailure::ReviewerUnconfigured)
                )
            ),
            "Review unavailable file"
        );
        assert_eq!(write.status.label_len, "Review unavailable".len());
        let held = |reason: &str| {
            format!(r#"{{"error":{{"type":"tool_review_held","reason":"{reason}","held":true}}}}"#)
        };
        let denied = |reason: &str| {
            format!(r#"{{"error":{{"type":"tool_permission_denied","reason":"{reason}"}}}}"#)
        };
        for (content, phrase) in [
            (held("review_caution"), "Safety caution rm -rf build"),
            (
                held("review_evidence_incomplete"),
                "Review evidence incomplete rm -rf build",
            ),
            (denied("auto_denied"), "Denied by auto agent rm -rf build"),
            (denied("policy_denied"), "Denied rm -rf build"),
            (
                denied("permission_required"),
                "Permission required rm -rf build",
            ),
        ] {
            let mut shell = row(
                "shell",
                ToolActivity::Command,
                ("Running", "Ran", "rm -rf build"),
            );
            assert_eq!(
                finished(&mut shell, ToolResultStatus::Failure, &content),
                phrase
            );
            assert_eq!(shell.status.outcome, Some(ToolOutcome::Denied));
        }
    }

    #[test]
    fn subagent_rows_settle_with_the_childs_identity_and_outcome() {
        let failure = |code: &str| format!(r#"{{"ok":false,"result":null,"error_code":"{code}"}}"#);
        let done = r#"{"ok":true,"result":"done","error_code":null}"#.to_owned();
        let cases = [
            (
                RUN_CHILD,
                ToolResultStatus::Success,
                done.clone(),
                "Subagent finished · inspect auth",
                "Subagent finished",
                ToolOutcome::Completed,
            ),
            (
                MESSAGE_CHILD,
                ToolResultStatus::Success,
                done,
                "reviewer replied · check this",
                "reviewer replied",
                ToolOutcome::Completed,
            ),
            (
                RUN_CHILD,
                ToolResultStatus::Failure,
                failure("child_failed"),
                "Subagent failed · inspect auth",
                "Subagent failed",
                ToolOutcome::Failed,
            ),
            (
                MESSAGE_CHILD,
                ToolResultStatus::Failure,
                failure("child_busy"),
                "reviewer busy; message not sent · check this",
                "reviewer busy; message not sent",
                ToolOutcome::Failed,
            ),
            (
                MESSAGE_CHILD,
                ToolResultStatus::Failure,
                failure("override_after_create"),
                "Message not sent to reviewer · check this",
                "Message not sent to reviewer",
                ToolOutcome::Failed,
            ),
            (
                RUN_CHILD,
                ToolResultStatus::Failure,
                failure("child_cancelled"),
                "Subagent interrupted · inspect auth",
                "Subagent interrupted",
                ToolOutcome::Failed,
            ),
            (
                RUN_CHILD,
                ToolResultStatus::Failure,
                tool_permission_denied_json("subagent"),
                "Denied Subagent · inspect auth",
                "Denied Subagent",
                ToolOutcome::Denied,
            ),
        ];
        for (arguments, status, content, phrase, label, outcome) in cases {
            let mut child = subagent_row(arguments);
            assert!(child.is_active());
            child.finish(&Finished {
                arguments,
                status,
                content: &content,
                process: None,
                status_detail: None,
                file_change: None,
            });
            assert_eq!(child.status.phrase, phrase, "{content}");
            assert_eq!(child.status.label_len, label.len(), "{content}");
            assert_eq!(child.status.outcome, Some(outcome), "{content}");
        }
    }

    fn status(model: &str, effort: ReasoningEffort) -> SubagentStatus {
        SubagentStatus {
            model: model.to_owned(),
            effort,
        }
    }

    #[test]
    fn a_subagent_row_keeps_its_childs_status_until_it_is_cancelled() {
        let mut child = subagent_row(RUN_CHILD);
        assert_eq!(child.child_status(), None);
        child.report_child(&status(
            "openai/gpt-5.5",
            ReasoningEffort::Named("high".to_owned()),
        ));
        assert_eq!(child.child_status(), Some("gpt-5.5 · high"));
        child.report_child(&status(
            "anthropic/claude-sonnet-4-5",
            ReasoningEffort::Auto,
        ));
        assert_eq!(child.child_status(), Some("sonnet 4-5"));
        child.finish(&Finished {
            arguments: RUN_CHILD,
            status: ToolResultStatus::Success,
            content: r#"{"ok":true,"result":"done","error_code":null}"#,
            process: None,
            status_detail: None,
            file_change: None,
        });
        assert_eq!(child.child_status(), Some("sonnet 4-5"));
        let mut cancelled = subagent_row(RUN_CHILD);
        cancelled.report_child(&status("gpt-5.5", ReasoningEffort::Auto));
        cancelled.cancel();
        assert_eq!(cancelled.child_status(), None);
        let mut hostile = subagent_row(RUN_CHILD);
        hostile.report_child(&status("evil\u{1b}[2J", ReasoningEffort::Auto));
        assert_eq!(hostile.child_status(), Some("evil\\x1b[2J"));
        let mut read = row("read_file", ToolActivity::Read, ("Reading", "Read", "a"));
        read.report_child(&status("gpt-5.5", ReasoningEffort::Auto));
        assert_eq!(read.child_status(), None);
    }

    #[test]
    fn a_cancelled_subagent_row_names_the_child() {
        for (arguments, target) in [
            (RUN_CHILD, "Subagent · inspect auth"),
            (MESSAGE_CHILD, "reviewer · check this"),
        ] {
            let mut child = subagent_row(arguments);
            assert_eq!(child.status.phrase, target.replacen(" ·", " working ·", 1));
            child.cancel();
            assert_eq!(child.status.phrase, format!("Cancelled {target}"));
            assert_eq!(child.status.outcome, Some(ToolOutcome::Cancelled));
            assert_eq!(
                child.cancellation_target(),
                Some((target.to_owned(), false))
            );
        }
    }

    #[test]
    fn command_process_outcomes_replace_the_completed_label() {
        let cases = [
            (
                CommandProcessPresentation::ExitCode(7),
                "Exited 7 exit 7",
                ToolOutcome::Failed,
            ),
            (
                CommandProcessPresentation::ExitCode(0),
                "Ran exit 7",
                ToolOutcome::Completed,
            ),
            (
                CommandProcessPresentation::Signal(9),
                "Signaled 9 exit 7",
                ToolOutcome::Failed,
            ),
            (
                CommandProcessPresentation::TimedOut,
                "Timed out exit 7",
                ToolOutcome::Failed,
            ),
        ];
        for (process, phrase, outcome) in cases {
            let mut shell = row("shell", ToolActivity::Command, ("Running", "Ran", "exit 7"));
            assert_eq!(
                finish_with(
                    &mut shell,
                    ToolResultStatus::Success,
                    "",
                    Some(process),
                    None
                ),
                phrase
            );
            assert_eq!(shell.status.outcome, Some(outcome));
            assert_eq!(shell.status.process, Some(process));
        }
        let mut observe = row(
            "shell",
            ToolActivity::Command,
            ("Waiting for", "Observed", "sleep 3"),
        );
        assert_eq!(
            finished(&mut observe, ToolResultStatus::Success, ""),
            "Observed sleep 3"
        );
        let mut read = row("read_file", ToolActivity::Read, ("Reading", "Read", "a"));
        assert_eq!(
            finish_with(
                &mut read,
                ToolResultStatus::Success,
                "",
                Some(CommandProcessPresentation::ExitCode(3)),
                None
            ),
            "Read a"
        );
    }

    #[test]
    fn failures_show_preflight_details_and_shell_corrections() {
        let mut grep = row(
            "grep_files",
            ToolActivity::Read,
            ("Searching", "Searched", "zzz"),
        );
        assert_eq!(
            finish_with(
                &mut grep,
                ToolResultStatus::Failure,
                "Path not found: nope",
                None,
                Some(ToolStatusDetail::PreflightFailed)
            ),
            "Failed zzz: Path not found: nope"
        );
        let mut read = row(
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", "README.md"),
        );
        assert_eq!(
            finished(
                &mut read,
                ToolResultStatus::Failure,
                "read_file field \"start_line\" must be positive"
            ),
            "Failed README.md"
        );
        let mut edit = row(
            "edit_file",
            ToolActivity::Edit,
            ("Editing", "Edited", "README.md"),
        );
        assert_eq!(
            finish_with(
                &mut edit,
                ToolResultStatus::Failure,
                "edit_file failed: old_string not found in file.",
                None,
                Some(ToolStatusDetail::PreflightFailed)
            ),
            "Failed README.md: old_string not found in file."
        );
        let mut stale = row(
            "write_file",
            ToolActivity::Write,
            ("Writing", "Wrote", "a.txt"),
        );
        assert_eq!(
            finish_with(
                &mut stale,
                ToolResultStatus::Failure,
                "changed",
                None,
                Some(ToolStatusDetail::StalePreview)
            ),
            "Failed a.txt: stale preview"
        );
    }

    #[test]
    fn failure_details_are_masked_flattened_and_bounded() {
        let mut multiline = row("edit_file", ToolActivity::Edit, ("Editing", "Edited", "a"));
        assert_eq!(
            finish_with(
                &mut multiline,
                ToolResultStatus::Failure,
                "edit_file failed: one\ntwo\x1b[2J",
                None,
                Some(ToolStatusDetail::PreflightFailed)
            ),
            "Failed a: edit_file failed: one two\\x1b[2J"
        );
        let mut empty = row("read_file", ToolActivity::Read, ("Reading", "Read", "a"));
        assert_eq!(
            finish_with(
                &mut empty,
                ToolResultStatus::Failure,
                "",
                None,
                Some(ToolStatusDetail::PreflightFailed)
            ),
            "Failed a: preflight failed"
        );
        let mut secret = row("read_file", ToolActivity::Read, ("Reading", "Read", "a"));
        let phrase = finish_with(
            &mut secret,
            ToolResultStatus::Failure,
            "Path not found: sk-proj-abcdefghijklmnopqrstuvwxyz0123456789",
            None,
            Some(ToolStatusDetail::PreflightFailed),
        );
        assert!(phrase.contains("[redacted]"), "{phrase}");
        let long = "x".repeat(1000);
        let mut bounded = row("read_file", ToolActivity::Read, ("Reading", "Read", "a"));
        let phrase = finish_with(
            &mut bounded,
            ToolResultStatus::Failure,
            &long,
            None,
            Some(ToolStatusDetail::PreflightFailed),
        );
        assert_eq!(phrase.len(), "Failed a: ".len() + MAX_FAILURE_DETAIL_BYTES);
        let correction =
            r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":["a"]}}"#;
        let mut shell = row(
            "shell",
            ToolActivity::Command,
            ("Running", "Ran", "echo hi"),
        );
        assert_eq!(
            finished(&mut shell, ToolResultStatus::Failure, correction),
            "Failed echo hi · 1 invalid field"
        );
    }

    #[test]
    fn long_commands_keep_the_activity_bound_before_a_suffix() {
        let command = "y".repeat(300);
        let bounded = format!("{}...", "y".repeat(117));
        let mut shell = ToolActivityRow::started(
            ToolCallId::new("call"),
            "shell",
            description(
                ToolActivity::Command,
                Some(("Running", "Ran", &command)),
                &format!("Running {bounded}"),
            ),
        );
        assert_eq!(shell.status.phrase, format!("Running {command}"));
        let correction =
            r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":["a","b"]}}"#;
        assert_eq!(
            finished(&mut shell, ToolResultStatus::Failure, correction),
            format!("Failed {bounded} · 2 invalid fields")
        );
        let mut failed = ToolActivityRow::started(
            ToolCallId::new("call"),
            "shell",
            description(
                ToolActivity::Command,
                Some(("Running", "Ran", &command)),
                &format!("Running {bounded}"),
            ),
        );
        assert_eq!(
            finished(&mut failed, ToolResultStatus::Failure, "boom"),
            format!("Failed {command}")
        );
    }

    #[test]
    fn rejections_fail_their_call_with_the_known_target() {
        let rejected = |reason, tool: &str, arguments, description, content| {
            ToolActivityRow::rejected(
                ToolCallId::new("call"),
                tool,
                Rejected {
                    reason,
                    arguments,
                    description,
                    content,
                },
            )
            .status
            .phrase
        };
        assert_eq!(
            rejected(ToolRejection::Unsupported, "no_such_tool", "{}", None, ""),
            "Failed no_such_tool"
        );
        assert_eq!(
            rejected(ToolRejection::Unsupported, "evil\x1b[2J", "{}", None, ""),
            "Failed evil\\x1b[2J"
        );
        assert_eq!(
            rejected(ToolRejection::Panicked, "read_file", "{}", None, ""),
            "Failed read_file"
        );
        assert_eq!(
            rejected(
                ToolRejection::MalformedArguments,
                "read_file",
                "{\"path\":",
                None,
                ""
            ),
            "Failed tool call: invalid JSON arguments"
        );
        assert_eq!(
            rejected(
                ToolRejection::MalformedArguments,
                "glob_files",
                "[1]",
                None,
                ""
            ),
            "Failed tool call: non-object arguments"
        );
        assert_eq!(
            rejected(
                ToolRejection::MalformedArguments,
                "write_file",
                "[1]",
                None,
                ""
            ),
            "Failed file: non-object arguments"
        );
        let correction =
            r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":["a"]}}"#;
        assert_eq!(
            rejected(
                ToolRejection::Invalid,
                "shell",
                "{}",
                Some(description(
                    ToolActivity::Command,
                    Some(("Running", "Ran", "echo hi")),
                    "Running echo hi"
                )),
                correction
            ),
            "Failed echo hi · 1 invalid field"
        );
        assert_eq!(
            rejected(
                ToolRejection::Invalid,
                "skill",
                "{}",
                Some(description(
                    ToolActivity::Read,
                    Some(("Loading skill", "Loaded skill", "x")),
                    "Loading skill x"
                )),
                "nope"
            ),
            "Failed x"
        );
    }

    #[test]
    fn calls_the_project_gate_never_ran_name_why() {
        let mut write = row(
            "write_file",
            ToolActivity::Write,
            ("Writing", "Wrote", "file"),
        );
        write.defer(ToolDeferral::TargetChanged);
        assert_eq!(write.status.phrase, "Not executed file");
        assert_eq!(write.status.outcome, Some(ToolOutcome::Denied));
        write.cancel();
        write.defer(ToolDeferral::ProjectInstructions);
        assert_eq!(write.status.phrase, "Not executed file");
        let mut read = row(
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", "runtime.zig"),
        );
        read.defer(ToolDeferral::ProjectInstructions);
        assert_eq!(
            read.status.phrase,
            "Reading project instructions before continuing: runtime.zig"
        );
        assert_eq!(read.status.outcome, Some(ToolOutcome::Deferred));
    }

    fn saved(
        tool: &str,
        description: Option<CallDescription>,
        status: ToolResultStatus,
        output: &str,
    ) -> ToolStatus {
        ToolActivityRow::saved(SavedToolCall {
            call_id: ToolCallId::new("call"),
            tool_name: tool.to_owned(),
            arguments: "{}".to_owned(),
            description,
            status,
            output: output.to_owned(),
            process: None,
        })
        .status
    }

    #[test]
    fn saved_calls_settle_with_upstream_resume_labels_and_no_details() {
        let read = || {
            Some(description(
                ToolActivity::Read,
                Some(("Reading", "Read", "a.md")),
                "Reading a.md",
            ))
        };
        let shell = Some(description(
            ToolActivity::Command,
            Some(("Running", "Ran", "ls")),
            "Running ls",
        ));
        let invalid_fields =
            r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":["a"]}}"#;
        let cases = [
            (
                saved("read_file", read(), ToolResultStatus::Success, "hello"),
                "Read a.md",
                ToolOutcome::Completed,
            ),
            (
                saved(
                    "read_file",
                    read(),
                    ToolResultStatus::Failure,
                    "Path not found: a.md",
                ),
                "Failed a.md",
                ToolOutcome::Failed,
            ),
            (
                saved("shell", shell, ToolResultStatus::Failure, invalid_fields),
                "Failed ls",
                ToolOutcome::Failed,
            ),
            (
                saved(
                    "read_file",
                    read(),
                    ToolResultStatus::Failure,
                    &tool_permission_denied_json("read_file"),
                ),
                "Denied a.md",
                ToolOutcome::Denied,
            ),
            (
                saved(
                    "read_file",
                    read(),
                    ToolResultStatus::Failure,
                    DEFERRED_TOOL_OUTPUT,
                ),
                "Not executed a.md",
                ToolOutcome::Denied,
            ),
            (
                saved(
                    "read_file",
                    read(),
                    ToolResultStatus::Failure,
                    CONTEXT_DEFERRED_TOOL_OUTPUT,
                ),
                "Reading project instructions before continuing: a.md",
                ToolOutcome::Deferred,
            ),
            (
                saved(
                    "read_file",
                    read(),
                    ToolResultStatus::Success,
                    DEFERRED_TOOL_OUTPUT,
                ),
                "Read a.md",
                ToolOutcome::Completed,
            ),
            (
                saved("gone_tool", None, ToolResultStatus::Failure, "boom"),
                "Failed tool call",
                ToolOutcome::Failed,
            ),
        ];
        for (status, phrase, outcome) in cases {
            assert_eq!(status.phrase, phrase);
            assert_eq!(status.outcome, Some(outcome));
            assert_eq!(status.process, None);
        }
    }

    fn saved_command(
        arguments: &str,
        status: ToolResultStatus,
        output: &str,
        process: Option<CommandProcessPresentation>,
    ) -> ToolStatus {
        ToolActivityRow::saved(SavedToolCall {
            call_id: ToolCallId::new("call"),
            tool_name: "shell".to_owned(),
            arguments: arguments.to_owned(),
            description: Some(description(
                ToolActivity::Command,
                Some(("Running", "Ran", "ls")),
                "Running ls",
            )),
            status,
            output: output.to_owned(),
            process,
        })
        .status
    }

    #[test]
    fn saved_commands_settle_with_their_process_outcome_when_upstream_captured_them() {
        use CommandProcessPresentation::{ExitCode, OutputCaptureFailed, Signal, TimedOut};
        let run = r#"{"action":"run","command":"ls"}"#;
        let failed = ToolResultStatus::Failure;
        let ran = ToolResultStatus::Success;
        let wrapped = r#"{"request":{"action":"run","command":"ls"}}"#;
        let terminal = r#"{"action":"run","command":"ls","tty":true}"#;
        let cases = [
            (
                run,
                failed,
                Some(ExitCode(3)),
                "Exited 3 ls",
                ToolOutcome::Failed,
            ),
            (
                run,
                failed,
                Some(Signal(9)),
                "Signaled 9 ls",
                ToolOutcome::Failed,
            ),
            (
                run,
                failed,
                Some(TimedOut),
                "Timed out ls",
                ToolOutcome::Failed,
            ),
            (
                run,
                failed,
                Some(OutputCaptureFailed),
                "Output capture failed ls",
                ToolOutcome::Failed,
            ),
            (
                run,
                ran,
                Some(ExitCode(0)),
                "Ran ls",
                ToolOutcome::Completed,
            ),
            (run, ran, None, "Ran ls", ToolOutcome::Completed),
            (
                wrapped,
                failed,
                Some(ExitCode(3)),
                "Failed ls",
                ToolOutcome::Failed,
            ),
            (
                terminal,
                failed,
                Some(ExitCode(3)),
                "Failed ls",
                ToolOutcome::Failed,
            ),
        ];
        for (arguments, status, process, phrase, outcome) in cases {
            let settled = saved_command(arguments, status, "", process);
            assert_eq!(settled.phrase, phrase, "{arguments} {process:?}");
            assert_eq!(settled.outcome, Some(outcome), "{arguments} {process:?}");
        }
        let denied = saved_command(
            run,
            failed,
            &tool_permission_denied_json("shell"),
            Some(ExitCode(3)),
        );
        assert_eq!(denied.phrase, "Denied ls");
        assert_eq!(
            saved_command(run, failed, "", Some(ExitCode(3))).process,
            Some(ExitCode(3))
        );
    }

    #[test]
    fn saved_subagent_calls_settle_as_upstream_resume_names_them() {
        let saved_child = |arguments: &str, status: ToolResultStatus, output: &str| {
            let started = subagent_row(arguments);
            let description = CallDescription {
                title: started.title.clone(),
                label: None,
                activity: ToolActivity::Subagent,
                effect: ToolEffect::Mutating,
                concurrency: Concurrency::Parallel,
            };
            ToolActivityRow::saved(SavedToolCall {
                call_id: ToolCallId::new("call"),
                tool_name: "subagent".to_owned(),
                arguments: arguments.to_owned(),
                description: Some(description),
                status,
                output: output.to_owned(),
                process: None,
            })
            .status
        };
        let cases = [
            (
                RUN_CHILD,
                ToolResultStatus::Success,
                r#"{"ok":true,"result":"done","error_code":null}"#.to_owned(),
                "Subagent finished · inspect auth",
                ToolOutcome::Completed,
            ),
            (
                RUN_CHILD,
                ToolResultStatus::Success,
                r#"{"ok":true,"pending":true}"#.to_owned(),
                "Subagent still running · inspect auth",
                ToolOutcome::Completed,
            ),
            (
                MESSAGE_CHILD,
                ToolResultStatus::Success,
                r#"{"ok":true,"delivery":"queued"}"#.to_owned(),
                "reviewer feedback queued · check this",
                ToolOutcome::Completed,
            ),
            (
                MESSAGE_CHILD,
                ToolResultStatus::Success,
                r#"{"ok":true,"delivery":"not_applied"}"#.to_owned(),
                "reviewer replied · check this",
                ToolOutcome::Completed,
            ),
            (
                MESSAGE_CHILD,
                ToolResultStatus::Failure,
                r#"{"ok":false,"error_code":"child_busy"}"#.to_owned(),
                "reviewer busy; message not sent · check this",
                ToolOutcome::Failed,
            ),
            (
                RUN_CHILD,
                ToolResultStatus::Failure,
                tool_permission_denied_json("subagent"),
                "Denied Subagent · inspect auth",
                ToolOutcome::Denied,
            ),
            (
                RUN_CHILD,
                ToolResultStatus::Failure,
                DEFERRED_TOOL_OUTPUT.to_owned(),
                "Not executed Subagent · inspect auth",
                ToolOutcome::Denied,
            ),
        ];
        for (arguments, status, output, phrase, outcome) in cases {
            let settled = saved_child(arguments, status, &output);
            assert_eq!(settled.phrase, phrase, "{output}");
            assert_eq!(settled.outcome, Some(outcome), "{output}");
        }
    }

    #[test]
    fn cancellation_and_abandonment_settle_only_active_rows() {
        let mut shell = row(
            "shell",
            ToolActivity::Command,
            ("Running", "Ran", "sleep 8"),
        );
        shell.cancel();
        assert_eq!(shell.status.phrase, "Cancelled sleep 8");
        assert_eq!(shell.status.label_len, "Cancelled".len());
        assert_eq!(shell.status.outcome, Some(ToolOutcome::Cancelled));
        let mut done = row("read_file", ToolActivity::Read, ("Reading", "Read", "a"));
        finished(&mut done, ToolResultStatus::Success, "");
        done.cancel();
        done.abandon(TurnOutcome::Failed);
        assert_eq!(done.status.phrase, "Read a");
        for (outcome, phrase, settled) in [
            (
                TurnOutcome::Completed,
                "Tool completion was not reported",
                ToolOutcome::Unreported,
            ),
            (
                TurnOutcome::Interrupted,
                "Tool cancelled",
                ToolOutcome::Cancelled,
            ),
            (TurnOutcome::Failed, "Tool failed", ToolOutcome::Failed),
        ] {
            let mut active = row("read_file", ToolActivity::Read, ("Reading", "Read", "a"));
            active.abandon(outcome);
            assert_eq!(active.status.phrase, phrase);
            assert_eq!(active.status.label_len, phrase.len());
            assert_eq!(active.status.outcome, Some(settled));
        }
    }

    #[test]
    fn targets_are_terminal_safe_and_bounded() {
        let hostile = row(
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", "a\x1b]2;pwn\x07\u{202e}b"),
        );
        assert_eq!(hostile.status.phrase, "Reading a\\x1b]2;pwn\\x07\\u{202e}b");
        let huge = "z".repeat(MAX_TARGET_BYTES * 2);
        let bounded = row("read_file", ToolActivity::Read, ("Reading", "Read", &huge));
        assert_eq!(
            bounded.status.phrase.len(),
            "Reading ".len() + MAX_TARGET_BYTES
        );
        let untitled = ToolActivityRow::started(
            ToolCallId::new("call"),
            "shell",
            description(ToolActivity::Command, None, "Working: shell"),
        );
        assert_eq!(untitled.status.phrase, "Working: shell");
        assert_eq!(untitled.command_display(), None);
        let mut untitled = untitled;
        assert_eq!(
            finished(&mut untitled, ToolResultStatus::Failure, ""),
            "Failed tool call"
        );
    }
}
