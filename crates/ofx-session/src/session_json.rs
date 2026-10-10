use ofx_contract::FileEvidence;
use serde_json::{Map, Value, json};

use crate::session_event::{
    ArtifactCompleteness, CommittedFilePresentation, ToolCallEvent, ToolResultEvent, WireTag,
};
use crate::session_log::{ArchivedSteering, ExecutedStep, TurnExecution};

const PRESENTATION_SCHEMA_VERSION: u64 = 3;

impl TurnExecution {
    pub fn presentation_json(&self) -> Value {
        json!({
            "schema_version": PRESENTATION_SCHEMA_VERSION,
            "tool_steps": self.steps.iter().map(step_json).collect::<Vec<_>>(),
            "files": self.files.iter().map(file_json).collect::<Vec<_>>(),
            "steering": self.steering.iter().map(steering_json).collect::<Vec<_>>(),
        })
    }
}

impl ToolResultEvent {
    pub fn output(&self) -> &str {
        self.preview.as_deref().unwrap_or_default()
    }
}

fn step_json(step: &ExecutedStep) -> Value {
    json!({
        "assistant": step.assistant,
        "tool_calls": step.calls.iter().map(call_json).collect::<Vec<_>>(),
        "tool_results": step.results.iter().map(result_json).collect::<Vec<_>>(),
    })
}

fn call_json(call: &ToolCallEvent) -> Value {
    json!({
        "id": call.call_id,
        "name": call.tool_name,
        "arguments_json": call.arguments_json,
        "provider_result": call.provider_result,
    })
}

fn result_json(result: &ToolResultEvent) -> Value {
    let mut object = Map::new();
    let mut insert = |key: &str, value: Value| {
        object.insert(key.to_owned(), value);
    };
    insert("tool_call_id", json!(result.call_id));
    insert("tool_name", json!(result.tool_name));
    insert("status", json!(result.status.label()));
    insert("output", json!(result.output()));
    insert("output_handle", json!(result.artifact_ref));
    if let Some(preview) = &result.preview {
        insert("preview", json!(preview));
    }
    insert(
        "output_bytes",
        json!(result.output_bytes.unwrap_or(result.stored_bytes)),
    );
    insert("stored_output_bytes", json!(result.stored_bytes));
    insert(
        "truncated",
        json!(result.completeness != ArtifactCompleteness::Complete),
    );
    insert("provider_native", json!(result.provider_native));
    insert("created_at_ms", json!(result.created_at_ms));
    insert("permission_feedback", json!(result.permission_feedback));
    if let Some(presentation) = &result.committed_file_presentation {
        insert(
            "committed_file_presentation",
            presentation_json(presentation),
        );
    }
    Value::Object(object)
}

fn presentation_json(presentation: &CommittedFilePresentation) -> Value {
    let lines: Vec<Value> = presentation
        .lines
        .iter()
        .map(|line| {
            json!({
                "kind": line.kind.tag(),
                "old_line": line.old_line,
                "new_line": line.new_line,
                "text": line.text,
            })
        })
        .collect();
    json!({
        "path": presentation.path,
        "kind": presentation.kind.tag(),
        "lines": lines,
        "additions": presentation.additions,
        "deletions": presentation.deletions,
        "truncated": presentation.truncated,
        "previous_content": presentation.previous_content,
        "after_content": presentation.after_content,
        "lifecycle_id": presentation.lifecycle_id.as_ref().map(|id| json!({
            "turn_id": id.turn_id,
            "call_id": id.call_id,
        })),
        "content_handle": presentation.content_handle,
    })
}

fn file_json(file: &FileEvidence) -> Value {
    json!({
        "path": file.path,
        "new_path": file.new_path,
        "tool_call_id": file.tool_call_id,
        "tool_name": file.tool_name,
        "action": file.action.label(),
        "status": file.status.label(),
        "model_view_covers_full_file": file.model_view_covers_full_file,
        "stale": file.stale,
    })
}

fn steering_json(steering: &ArchivedSteering) -> Value {
    json!({
        "text": steering.text,
        "assistant_prefix": steering.assistant_prefix,
        "after_tool_step_count": steering.after_tool_step_count,
    })
}

#[cfg(test)]
mod tests;
