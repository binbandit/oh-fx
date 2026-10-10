use ofx_contract::{
    ExecutionFailure, PersistedResult, ToolArgumentIntegrity, ToolCall, ToolExecutionProvenance,
    ToolResultStatus, tool_execution_failure_json,
};

use super::durable::durable_text;
use super::presentation::{read_command_output_replay, read_file_presentation};
use super::{SavedToolResult, SavedToolStep, fixed, list, tag, tool_images};
use crate::fixed_field::Null;
use crate::json_fields::{Fields, Json};
use crate::process_presentation;
use crate::session_event::saved_replay;

const MALFORMED_ARGUMENTS: &str = "Tool arguments were not valid JSON.";
const MALFORMED_ARGUMENTS_SUGGESTION: &str =
    "Reissue the tool call with complete valid JSON arguments matching the tool schema.";
const EMPTY_ARGUMENTS: &str = "{}";

pub(super) fn tool_step(value: Json<'_>) -> Option<SavedToolStep> {
    let mut fields = Fields::new(value)?;
    let assistant = fields.present_or_null("assistant", |value| durable_text(value).map(Some))?;
    let provider_replay = fields.present_or_null("provider_replay", |value| {
        saved_replay(value).map(|replay| Some(replay.into_provider_replay()))
    })?;
    let mut calls = list(fields.required("tool_calls")?, tool_call)?;
    let mut tool_results = list(fields.required("tool_results")?, tool_result)?;
    repair_arguments(&mut calls, &mut tool_results)?;
    let step = SavedToolStep {
        assistant,
        provider_replay,
        tool_calls: calls.into_iter().map(|(call, _)| call).collect(),
        tool_results,
    };
    let answers_its_calls = step.tool_results.iter().enumerate().all(|(index, result)| {
        let called = step
            .tool_calls
            .iter()
            .any(|call| call.id.as_str() == result.tool_call_id && call.name == result.tool_name);
        let repeated = step.tool_results[..index]
            .iter()
            .any(|seen| seen.tool_call_id == result.tool_call_id);
        called && !repeated
    });
    answers_its_calls.then_some(())?;
    fields.finish(step)
}

fn tool_call(value: Json<'_>) -> Option<(ToolCall, ToolArgumentIntegrity)> {
    let mut fields = Fields::new(value)?;
    let id = durable_text(fields.required("id")?)?;
    let name = durable_text(fields.required("name")?)?;
    let mut arguments = durable_text(fields.required("arguments_json")?)?;
    let integrity = ToolArgumentIntegrity::classify_function_input(&arguments);
    if integrity == ToolArgumentIntegrity::MalformedJson {
        EMPTY_ARGUMENTS.clone_into(&mut arguments);
    }
    let call = ToolCall {
        provider_result: fields
            .present_or_null("provider_result", |value| durable_text(value).map(Some))?,
        ..ToolCall::new(id, name, arguments)
    };
    fields.finish((call, integrity))
}

fn repair_arguments(
    calls: &mut [(ToolCall, ToolArgumentIntegrity)],
    results: &mut [SavedToolResult],
) -> Option<()> {
    let mut repairs = Vec::new();
    for (index, (call, integrity)) in calls.iter().enumerate() {
        if *integrity == ToolArgumentIntegrity::Valid {
            continue;
        }
        let same_id = |id: &str| id == call.id.as_str();
        let one_call = calls
            .iter()
            .filter(|(other, _)| same_id(other.id.as_str()))
            .count()
            == 1;
        let mut answers = results
            .iter()
            .enumerate()
            .filter(|(_, result)| same_id(&result.tool_call_id));
        let (answer, result) = answers.next()?;
        let named = result.tool_name == call.name;
        (one_call && named && answers.next().is_none()).then_some(())?;
        repairs.push((index, answer));
    }
    for (index, answer) in repairs {
        let (call, integrity) = &mut calls[index];
        let result = &mut results[answer];
        if *integrity == ToolArgumentIntegrity::NonObjectJson {
            if call.provider_result.is_some() || result.persisted.provider_native {
                call.provenance = ToolExecutionProvenance::ProviderExecuted;
            } else {
                EMPTY_ARGUMENTS.clone_into(&mut call.arguments);
            }
        } else {
            refuse_malformed(&call.name, result);
        }
        *integrity = ToolArgumentIntegrity::Valid;
    }
    Some(())
}

fn refuse_malformed(tool_name: &str, result: &mut SavedToolResult) {
    let output = tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: MALFORMED_ARGUMENTS,
        details: &[],
        suggestion: Some(MALFORMED_ARGUMENTS_SUGGESTION),
    });
    let bytes = output.len();
    result.status = ToolResultStatus::Failure;
    result.output = output;
    result.output_bytes = bytes;
    let persisted = &mut result.persisted;
    persisted.output_handle = None;
    persisted.preview = None;
    persisted.stored_output_bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    persisted.truncated = false;
    persisted.provider_native = false;
}

fn tool_result(value: Json<'_>) -> Option<SavedToolResult> {
    let mut fields = Fields::new(value)?;
    let tool_call_id = durable_text(fields.required("tool_call_id")?)?;
    let tool_name = durable_text(fields.required("tool_name")?)?;
    let status = tag(&fields.required("status")?)?;
    let provider_native = fields.flag("provider_native")?;
    let review_feedback = fields.flag("review_feedback")?;
    let held = status == ToolResultStatus::Failure && !provider_native;
    (!review_feedback || held).then_some(())?;
    let mut output = durable_text(fields.required("output")?)?;
    let output_handle =
        fields.present_or_null("output_handle", |value| durable_text(value).map(Some))?;
    let preview = fields.present_or_null("preview", |value| durable_text(value).map(Some))?;
    let permission_feedback = list(fields.required("permission_feedback")?, durable_text)?;
    let committed_file_presentation = fields
        .present_or_null("committed_file_presentation", |value| {
            read_file_presentation(value).map(Some)
        })?;
    let command_output_replay = fields.present_or_null("command_output_replay", |value| {
        read_command_output_replay(value).map(Some)
    })?;
    let process = fields.present_or_null("command_process_presentation", |value| {
        process_presentation::checkpoint::read(value).map(Some)
    })?;
    fixed::<Null>(&mut fields, "terminal_action_presentation")?;
    let tool_images = tool_images::read(&mut fields, &mut output)?;
    let result = SavedToolResult {
        tool_call_id,
        tool_name,
        status,
        output,
        output_bytes: usize::try_from(fields.unsigned("output_bytes")?).ok()?,
        process,
        review_feedback,
        permission_feedback,
        persisted: PersistedResult {
            output_handle,
            preview,
            stored_output_bytes: fields.unsigned("stored_output_bytes")?,
            truncated: fields.flag("truncated")?,
            created_at_ms: fields.signed("created_at_ms")?,
            provider_native,
            committed_file_presentation,
            command_output_replay,
            tool_images,
        },
    };
    fields.finish(result)
}
