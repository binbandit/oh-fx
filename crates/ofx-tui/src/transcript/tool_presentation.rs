use ofx_contract::{
    ActionLabel, CallDescription, CommandProcessPresentation, FileChangeStats, ToolActivity,
    ToolArgsError, ToolCallId, ToolDeferral, ToolPermissionDenialReason, ToolRejection,
    ToolResultStatus, ToolStatusDetail, TurnOutcome, parse_tool_args_object,
    shell_request_invalid_field_count, tool_permission_denial_reason,
};
use ofx_text::{encode_terminal_safe, encode_terminal_safe_inline, mask_secrets};

const MAX_RUN_COMMAND_ACTIVITY_BYTES: usize = 120;
const MAX_TARGET_BYTES: usize = MAX_RUN_COMMAND_ACTIVITY_BYTES * MAX_RUN_COMMAND_ACTIVITY_BYTES - 1;
const MAX_FAILURE_DETAIL_BYTES: usize = 256;
const SHELL_TOOL: &str = "shell";
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
}

pub(crate) struct Finished<'a> {
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
        }
    }

    pub(crate) fn abandon(&mut self, outcome: TurnOutcome) {
        if !self.is_active() {
            return;
        }
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
            (None, _) => INVALID_ARGUMENTS_TARGET.to_owned(),
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
        Some(match process? {
            CommandProcessPresentation::ExitCode(0) => (ToolOutcome::Completed, "Ran".to_owned()),
            CommandProcessPresentation::ExitCode(code) => {
                (ToolOutcome::Failed, format!("Exited {code}"))
            }
            CommandProcessPresentation::Signal(signal) => {
                (ToolOutcome::Failed, format!("Signaled {signal}"))
            }
            CommandProcessPresentation::TimedOut => (ToolOutcome::Failed, "Timed out".to_owned()),
        })
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
        Concurrency, ReviewFailure, ReviewHold, ToolEffect, tool_permission_denied_json,
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
            status,
            content,
            process,
            status_detail,
            file_change: None,
        });
        row.status.phrase.clone()
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
