use std::fmt::Write as _;

use ofx_contract::{
    CommandProcessPresentation, ExecutionFailure, ToolArgumentIntegrity, ToolExecutionProvenance,
    ToolResultStatus, TurnSummary, is_captured_command, tool_execution_failure_json,
};

use super::legacy_presentation::{
    CancelledCommand, CommandReplay, LegacyPresentation, cancelled_command, command_replay,
    file_presentation,
};
use crate::json_fields::{Fields, Json};
use crate::session_authority::parse_identifier;
use crate::session_codec::recovery_checkpoint::{
    durable_bytes, durable_text, file_evidence, list, tag,
};
use crate::session_event::{
    FileEvidence, InterruptReason, SavedReplay, ToolCallEvent, saved_replay,
};
use crate::{process_presentation, turn_summary};

const NEWEST_EXECUTION_SCHEMA: u64 = 10;
const STEERING_SCHEMA: u64 = 6;
const STEERING_ENTRY_SCHEMA: u64 = 7;
const TURN_SUMMARY_SCHEMA: u64 = 5;
const PROVIDER_REPLAY_SCHEMA: u64 = 9;
const TOOL_IMAGE_SCHEMA: u64 = 8;
const REVIEW_FEEDBACK_SCHEMA: u64 = 10;
const PROCESS_SCHEMA: u64 = 3;
const TERMINAL_ACTION_SCHEMA: u64 = 4;
const MAX_WORK_ID_BYTES: usize = 128;
const HISTORICAL_COMMAND: &str =
    "[Historical command record: fx no longer owns or controls this process";
const UNREADABLE_ARGUMENTS: &str = "Tool arguments were not valid JSON.";
const UNREADABLE_ARGUMENTS_SUGGESTION: &str =
    "Reissue the tool call with complete valid JSON arguments matching the tool schema.";

pub(super) enum LegacyTurn {
    Conversation(Box<ConversationTurn>),
    Compacted(CompactedSummary),
}

pub(super) struct CompactedSummary {
    pub(super) summary: String,
    pub(super) removed_turn_count: usize,
    pub(super) compaction_count: usize,
}

pub(super) struct ConversationTurn {
    pub(super) user: String,
    pub(super) work_id: Option<String>,
    pub(super) execution: Execution,
    pub(super) close: TurnClose,
}

#[derive(Default)]
pub(super) struct Execution {
    pub(super) steps: Vec<Step>,
    pub(super) files: Vec<FileEvidence>,
    pub(super) steering: Vec<Steering>,
    pub(super) turn_summary: Option<TurnSummary>,
}

pub(super) struct Step {
    pub(super) assistant: Option<String>,
    pub(super) replay: Option<SavedReplay>,
    pub(super) calls: Vec<ToolCallEvent>,
    pub(super) results: Vec<SavedResult>,
}

pub(super) struct SavedResult {
    pub(super) call_id: String,
    pub(super) tool_name: String,
    pub(super) status: ToolResultStatus,
    pub(super) output: Vec<u8>,
    pub(super) output_handle: Option<String>,
    pub(super) preview: Option<String>,
    pub(super) output_bytes: u64,
    pub(super) stored_output_bytes: u64,
    pub(super) truncated: bool,
    pub(super) provider_native: bool,
    pub(super) created_at_ms: i64,
    pub(super) permission_feedback: Vec<String>,
    pub(super) process: Option<CommandProcessPresentation>,
    pub(super) presentation: Option<Box<LegacyPresentation>>,
    pub(super) replay: Option<CommandReplay>,
}

pub(super) struct Steering {
    pub(super) text: String,
    pub(super) assistant_prefix: Option<String>,
    pub(super) after_tool_step_count: usize,
}

pub(super) enum TurnClose {
    Replied(String, Option<SavedReplay>),
    Interrupted {
        reason: InterruptReason,
        partial: Option<String>,
        pending: Option<ToolCallEvent>,
        completed: Vec<String>,
        cancelled: Option<CancelledCommand>,
    },
}

pub(super) fn history_turn(value: Json<'_>) -> Option<LegacyTurn> {
    let mut fields = Fields::new(value)?;
    let turn = match fields.string("kind")?.as_str() {
        "assistant" => assistant(&mut fields)?,
        "background_command" => background_command(&mut fields)?,
        "interrupted" => interrupted(&mut fields)?,
        "compacted_summary" => LegacyTurn::Compacted(compacted_summary(&mut fields)?),
        _ => return None,
    };
    fields.finish(turn)
}

fn compacted_summary(fields: &mut Fields<'_>) -> Option<CompactedSummary> {
    let summary = durable_text(fields.required("summary")?)?;
    let removed_turn_count = usize::try_from(fields.unsigned("removed_turn_count")?).ok()?;
    let compaction_count = usize::try_from(fields.unsigned("compaction_count")?).ok()?;
    let root_messages = fields.required("root_user_messages");
    let root_complete = fields.required("root_user_messages_complete");
    let feedback = fields.required("permission_feedback");
    let feedback_complete = fields.required("permission_feedback_complete");
    let shaped = match (
        root_messages.is_some(),
        root_complete.is_some(),
        feedback.is_some(),
        feedback_complete.is_some(),
    ) {
        (true, true, feedback, complete) => feedback == complete,
        (_, false, false, false) => true,
        _ => false,
    };
    shaped.then_some(())?;
    for messages in [root_messages, feedback].into_iter().flatten() {
        list(messages, durable_bytes)?;
    }
    for complete in [root_complete, feedback_complete].into_iter().flatten() {
        complete.as_bool()?;
    }
    Some(CompactedSummary {
        summary,
        removed_turn_count,
        compaction_count,
    })
}

pub(super) fn is_valid_work_id(work_id: &str) -> bool {
    (1..=MAX_WORK_ID_BYTES).contains(&work_id.len()) && !work_id.contains('\0')
}

fn assistant(fields: &mut Fields<'_>) -> Option<LegacyTurn> {
    let (user, work_id) = user(fields.required("user")?)?;
    let reply = durable_text(fields.required("assistant")?)?;
    let execution = execution(fields.required("execution")?)?;
    let provider_replay = fields.nullable("provider_replay", |value| replay(value).map(Some))?;
    Some(LegacyTurn::Conversation(Box::new(ConversationTurn {
        user,
        work_id,
        execution,
        close: TurnClose::Replied(reply, provider_replay),
    })))
}

fn background_command(fields: &mut Fields<'_>) -> Option<LegacyTurn> {
    let (user, work_id) = user(fields.required("user")?)?;
    let log_path = durable_text(fields.required("log_path")?)?;
    let url = fields.present_or_null("url", |value| durable_text(value).map(Some))?;
    fields.flag("expect_url")?;
    match fields.required("background_record_id")? {
        Json::Null => {}
        Json::String(id) => {
            parse_identifier(&id)?;
        }
        _ => return None,
    }
    let (reply, execution) = if let Some(saved) = fields.required("execution") {
        let reply = fields.present_or_null("assistant", |value| durable_text(value).map(Some))?;
        (reply, execution(saved)?)
    } else {
        (None, Execution::default())
    };
    Some(LegacyTurn::Conversation(Box::new(ConversationTurn {
        user,
        work_id,
        execution,
        close: TurnClose::Replied(
            historical_command(reply.as_deref(), &log_path, url.as_deref()),
            None,
        ),
    })))
}

fn historical_command(reply: Option<&str>, log_path: &str, url: Option<&str>) -> String {
    let mut text = String::new();
    if let Some(reply) = reply.filter(|reply| !reply.is_empty()) {
        text.push_str(reply);
        if !reply.ends_with('\n') {
            text.push('\n');
        }
    }
    text.push_str(HISTORICAL_COMMAND);
    if !log_path.is_empty() {
        let _ = write!(text, "; former log={log_path}");
    }
    if let Some(url) = url {
        let _ = write!(text, "; recorded url={url}");
    }
    text.push(']');
    text
}

fn interrupted(fields: &mut Fields<'_>) -> Option<LegacyTurn> {
    let (user, work_id) = user(fields.required("user")?)?;
    let partial = fields.present_or_null("assistant", |value| durable_text(value).map(Some))?;
    let pending = match fields.required("tool_call")? {
        Json::Null => None,
        call => Some(interrupted_call(call)?),
    };
    let completed = list(fields.required("completed_tool_names")?, durable_bytes)?
        .into_iter()
        .map(|name| String::from_utf8_lossy(&name).into_owned())
        .collect();
    let reason = fields.or(
        "terminal_reason",
        InterruptReason::Cancelled,
        |value| match value.as_str()? {
            "cancelled" => Some(InterruptReason::Cancelled),
            "failed" => Some(InterruptReason::Failed),
            _ => None,
        },
    )?;
    fields.or("cancellation_origin", (), |value| {
        (value.as_str()? == "turn").then_some(())
    })?;
    let execution = fields.or("execution", Execution::default(), execution)?;
    let cancelled = fields.or("cancelled_command", None, |value| {
        cancelled_command(value).map(Some)
    })?;
    let captured = pending
        .as_ref()
        .is_some_and(|call| is_captured_command(&call.tool_name, &call.arguments_json));
    if cancelled.is_some() && (reason != InterruptReason::Cancelled || !captured) {
        return None;
    }
    Some(LegacyTurn::Conversation(Box::new(ConversationTurn {
        user,
        work_id,
        execution,
        close: TurnClose::Interrupted {
            reason,
            partial,
            pending,
            completed,
            cancelled,
        },
    })))
}

pub(super) fn user(value: Json<'_>) -> Option<(String, Option<String>)> {
    let mut fields = Fields::new(value)?;
    let text = durable_text(fields.required("text")?)?;
    match fields.required("images")? {
        Json::Array(images) if images.is_empty() => {}
        _ => return None,
    }
    let work_id = fields.or("work_id", None, |value| {
        value
            .as_str()
            .filter(|work_id| is_valid_work_id(work_id))
            .map(|work_id| Some(work_id.to_owned()))
    })?;
    fields.finish((text, work_id))
}

pub(super) fn execution(value: Json<'_>) -> Option<Execution> {
    let mut fields = Fields::new(value)?;
    let version = fields
        .unsigned("schema_version")
        .filter(|version| (1..=NEWEST_EXECUTION_SCHEMA).contains(version))?;
    let steps = list(fields.required("tool_steps")?, |step| {
        tool_step(step, version)
    })?;
    let files = list(fields.required("files")?, file_evidence)?;
    let steering = if version >= STEERING_SCHEMA {
        steering(fields.required("steering")?, version, steps.len())?
    } else {
        Vec::new()
    };
    let turn_summary = if version >= TURN_SUMMARY_SCHEMA {
        fields.present_or_null("turn_summary", |value| {
            turn_summary::read_checkpoint(value).map(Some)
        })?
    } else {
        None
    };
    fields.finish(Execution {
        steps,
        files,
        steering,
        turn_summary,
    })
}

fn steering(value: Json<'_>, version: u64, tool_steps: usize) -> Option<Vec<Steering>> {
    let entries = if version < STEERING_ENTRY_SCHEMA {
        list(value, |text| {
            Some(Steering {
                text: durable_text(text)?,
                assistant_prefix: None,
                after_tool_step_count: tool_steps,
            })
        })?
    } else {
        list(value, steering_entry)?
    };
    let ordered = entries
        .windows(2)
        .all(|pair| pair[0].after_tool_step_count <= pair[1].after_tool_step_count);
    let bounded = entries
        .iter()
        .all(|entry| entry.after_tool_step_count <= tool_steps);
    (ordered && bounded).then_some(entries)
}

fn steering_entry(value: Json<'_>) -> Option<Steering> {
    let mut fields = Fields::new(value)?;
    let entry = Steering {
        text: durable_text(fields.required("text")?)?,
        assistant_prefix: fields
            .present_or_null("assistant_prefix", |value| durable_text(value).map(Some))?,
        after_tool_step_count: usize::try_from(fields.unsigned("after_tool_step_count")?).ok()?,
    };
    fields.finish(entry)
}

fn tool_step(value: Json<'_>, version: u64) -> Option<Step> {
    let mut fields = Fields::new(value)?;
    let assistant = fields.present_or_null("assistant", |value| durable_text(value).map(Some))?;
    let mut calls = list(fields.required("tool_calls")?, tool_call)?;
    let mut results = list(fields.required("tool_results")?, |result| {
        tool_result(result, version)
    })?;
    let replay = if version >= PROVIDER_REPLAY_SCHEMA {
        fields.present_or_null("provider_replay", |value| replay(value).map(Some))?
    } else {
        None
    };
    repair_arguments(&mut calls, &mut results)?;
    fields.finish(Step {
        assistant,
        replay,
        calls,
        results,
    })
}

fn tool_call(value: Json<'_>) -> Option<ToolCallEvent> {
    let mut fields = Fields::new(value)?;
    let id = durable_text(fields.required("id")?)?;
    let name = durable_text(fields.required("name")?)?;
    let arguments = durable_text(fields.required("arguments_json")?)?;
    let integrity = ToolArgumentIntegrity::classify_function_input(&arguments);
    let arguments = if integrity == ToolArgumentIntegrity::MalformedJson {
        "{}".to_owned()
    } else {
        arguments
    };
    let mut call = ToolCallEvent::new(id, name, arguments, integrity);
    call.provider_result =
        fields.present_or_null("provider_result", |value| durable_text(value).map(Some))?;
    fields.finish(call)
}

fn interrupted_call(value: Json<'_>) -> Option<ToolCallEvent> {
    let mut call = tool_call(value)?;
    if call.argument_integrity == ToolArgumentIntegrity::NonObjectJson {
        if call.provider_result.is_some() {
            call.provenance = ToolExecutionProvenance::ProviderExecuted;
        } else {
            "{}".clone_into(&mut call.arguments_json);
        }
    }
    call.argument_integrity = ToolArgumentIntegrity::Valid;
    Some(call)
}

fn repair_arguments(calls: &mut [ToolCallEvent], results: &mut [SavedResult]) -> Option<()> {
    for index in 0..calls.len() {
        let integrity = calls[index].argument_integrity;
        if integrity == ToolArgumentIntegrity::Valid {
            continue;
        }
        let call = &calls[index];
        let named = calls
            .iter()
            .filter(|candidate| candidate.call_id == call.call_id)
            .count();
        let mut paired = results
            .iter_mut()
            .filter(|result| result.call_id == call.call_id);
        let result = paired.next()?;
        if named != 1 || paired.next().is_some() || result.tool_name != call.tool_name {
            return None;
        }
        let call = &mut calls[index];
        if integrity == ToolArgumentIntegrity::NonObjectJson {
            if call.provider_result.is_some() || result.provider_native {
                call.provenance = ToolExecutionProvenance::ProviderExecuted;
            } else {
                "{}".clone_into(&mut call.arguments_json);
            }
        } else {
            refuse_unreadable_arguments(result);
        }
        call.argument_integrity = ToolArgumentIntegrity::Valid;
    }
    Some(())
}

fn refuse_unreadable_arguments(result: &mut SavedResult) {
    let output = tool_execution_failure_json(&ExecutionFailure {
        tool_name: &result.tool_name,
        message: UNREADABLE_ARGUMENTS,
        details: &[],
        suggestion: Some(UNREADABLE_ARGUMENTS_SUGGESTION),
    });
    let bytes = u64::try_from(output.len()).unwrap_or(u64::MAX);
    result.status = ToolResultStatus::Failure;
    result.output = output.into_bytes();
    result.output_handle = None;
    result.preview = None;
    result.output_bytes = bytes;
    result.stored_output_bytes = bytes;
    result.truncated = false;
    result.provider_native = false;
}

fn tool_result(value: Json<'_>, version: u64) -> Option<SavedResult> {
    let mut fields = Fields::new(value)?;
    let mut result = SavedResult {
        call_id: durable_text(fields.required("tool_call_id")?)?,
        tool_name: durable_text(fields.required("tool_name")?)?,
        status: tag(&fields.required("status")?)?,
        output: durable_bytes(fields.required("output")?)?,
        output_handle: fields
            .present_or_null("output_handle", |value| durable_text(value).map(Some))?,
        preview: fields.present_or_null("preview", |value| durable_text(value).map(Some))?,
        output_bytes: fields.unsigned("output_bytes")?,
        stored_output_bytes: fields.unsigned("stored_output_bytes")?,
        truncated: fields.flag("truncated")?,
        provider_native: fields.flag("provider_native")?,
        created_at_ms: fields.signed("created_at_ms")?,
        permission_feedback: if version == 1 {
            Vec::new()
        } else {
            list(fields.required("permission_feedback")?, durable_text)?
        },
        process: if version >= PROCESS_SCHEMA {
            fields.present_or_null("command_process_presentation", |value| {
                process_presentation::checkpoint::read(value).map(Some)
            })?
        } else {
            None
        },
        presentation: None,
        replay: None,
    };
    let presentation = |value| file_presentation(value).map(|shown| Some(Box::new(shown)));
    result.presentation = match version {
        1 => None,
        2 => fields.nullable("committed_file_presentation", presentation)?,
        _ => fields.present_or_null("committed_file_presentation", presentation)?,
    };
    if version >= PROCESS_SCHEMA {
        result.replay = fields.present_or_null("command_output_replay", |value| {
            command_replay(value).map(Some)
        })?;
    }
    if version >= TERMINAL_ACTION_SCHEMA {
        absent(&fields.required("terminal_action_presentation")?)?;
    }
    if version >= TOOL_IMAGE_SCHEMA {
        fields.or("tool_image_handle", (), |value| absent(&value))?;
        if fields.required("tool_images").is_some() {
            return None;
        }
    }
    if version >= REVIEW_FEEDBACK_SCHEMA && fields.flag("review_feedback")? {
        return None;
    }
    fields.finish(result)
}

fn replay(value: Json<'_>) -> Option<SavedReplay> {
    saved_replay(value).filter(SavedReplay::is_valid)
}

fn absent(value: &Json<'_>) -> Option<()> {
    value.is_null().then_some(())
}
