use std::borrow::Cow;

use ofx_contract::{
    FileEvidence, FileEvidenceAction, HistoryStep, StepResult, ToolCall, ToolResultStatus,
};
use ofx_text::{lowercase_hex, mask_secrets};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const REDACTED_DIGEST_BYTES: usize = 12;

#[derive(Debug, Default)]
pub(crate) struct EarlierEvidence {
    files: Vec<FileEvidence>,
    recovered_results: usize,
}

impl EarlierEvidence {
    pub(crate) fn recovered(files: Vec<FileEvidence>, recovered_results: usize) -> Self {
        Self {
            files,
            recovered_results,
        }
    }

    pub(crate) fn turn_files(&self, steps: &[HistoryStep<'_>]) -> Vec<FileEvidence> {
        let mut files = self.files.clone();
        files.extend(steps_file_evidence(steps, self.recovered_results));
        mark_stale(&mut files);
        files
    }

    pub(crate) fn keep_compacted(&mut self, steps: &[HistoryStep<'_>]) {
        let compacted = steps_file_evidence(steps, self.recovered_results);
        self.files.extend(compacted);
        let results = steps.iter().map(|step| step.tool_results.len()).sum();
        self.recovered_results = self.recovered_results.saturating_sub(results);
    }
}

fn steps_file_evidence<'a>(
    steps: &'a [HistoryStep<'a>],
    recovered_results: usize,
) -> impl Iterator<Item = FileEvidence> + 'a {
    steps
        .iter()
        .flat_map(|step| step.tool_results.iter().map(move |result| (step, result)))
        .skip(recovered_results)
        .filter_map(|(step, result)| {
            let call = step
                .tool_calls
                .iter()
                .find(|call| call.id.as_str() == result.call_id)?;
            file_evidence(call, result)
        })
}

fn file_evidence(call: &ToolCall, result: &StepResult<'_>) -> Option<FileEvidence> {
    let action = action_for_tool(&call.name)?;
    let arguments: Value = serde_json::from_str(&call.arguments).ok()?;
    let arguments = arguments.as_object()?;
    let path = match action {
        FileEvidenceAction::Search => ["path", "directory", "query"]
            .into_iter()
            .find_map(|name| non_empty(arguments, name))
            .unwrap_or("."),
        _ => non_empty(arguments, "path")?,
    };
    let succeeded = result.status == ToolResultStatus::Success;
    Some(FileEvidence {
        path: path.to_owned(),
        new_path: None,
        tool_call_id: durable_identifier(call.id.as_str()),
        tool_name: call.name.clone(),
        action,
        status: result.status,
        model_view_covers_full_file: succeeded
            && action == FileEvidenceAction::Read
            && result.model_view_covers_full_file,
        stale: false,
    })
}

fn mark_stale(files: &mut [FileEvidence]) {
    for index in 0..files.len() {
        let changed = &files[index];
        if changed.status != ToolResultStatus::Success
            || !matches!(
                changed.action,
                FileEvidenceAction::Write | FileEvidenceAction::Edit
            )
        {
            continue;
        }
        let path = changed.path.clone();
        for prior in &mut files[..index] {
            if prior.action == FileEvidenceAction::Read && prior.path == path {
                prior.stale = true;
            }
        }
    }
}

fn action_for_tool(name: &str) -> Option<FileEvidenceAction> {
    match name {
        "read_file" => Some(FileEvidenceAction::Read),
        "write_file" => Some(FileEvidenceAction::Write),
        "edit_file" => Some(FileEvidenceAction::Edit),
        "grep_files" | "glob_files" => Some(FileEvidenceAction::Search),
        _ => None,
    }
}

fn non_empty<'a>(arguments: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn durable_identifier(value: &str) -> String {
    if matches!(mask_secrets(value), Cow::Borrowed(_)) {
        return value.to_owned();
    }
    let digest = Sha256::digest(value.as_bytes());
    format!(
        "redacted-{}",
        lowercase_hex(&digest[..REDACTED_DIGEST_BYTES])
    )
}

#[cfg(test)]
mod tests;
