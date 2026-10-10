use ofx_config::EMERGENCY_CEILING_BYTES;
use ofx_contract::{CommandProcessPresentation, TurnSummary};
use serde::Serialize;

use super::durable_turn::{SavedResult, Steering, Step};
use super::legacy_checkpoint::Continuable;
use crate::fixed_field::{False, NoItems, Null};
use crate::result_store::{PREVIEW_BYTES, STORED_TEXT_MAX_BYTES, make_handle, preview};
use crate::session_codec::SavedProvider;
use crate::session_codec::recovery_checkpoint::{
    CHECKPOINT_VERSION, EXECUTION_SCHEMA_VERSION, decode_recovery_file,
};
use crate::session_error::SessionError;
use crate::session_event::{FileEvidence, WireTag};
use crate::{process_presentation, turn_summary};

pub(super) struct RecoveryFile {
    pub(super) bytes: Vec<u8>,
    pub(super) spilled: Vec<(String, String)>,
}

#[derive(Serialize)]
struct CheckpointWire<'a> {
    version: u64,
    turn_id: u64,
    user: UserWire<'a>,
    assistant_source: &'a str,
    execution: ExecutionWire<'a>,
    cause: &'static str,
    action: &'static str,
    tool_state: &'static str,
    authority: AuthorityWire<'a>,
    requested_fast_mode: bool,
    fast_mode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_ultrafast_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ultrafast_mode: Option<bool>,
    max_provider_attempts: u64,
    consumed_provider_attempts: u64,
    outstanding_reservation: bool,
}

#[derive(Serialize)]
struct UserWire<'a> {
    text: &'a str,
    images: NoItems,
}

#[derive(Serialize)]
struct ExecutionWire<'a> {
    schema_version: u64,
    tool_steps: Vec<StepWire<'a>>,
    files: Vec<&'a FileEvidence>,
    steering: Vec<SteeringWire<'a>>,
    #[serde(serialize_with = "turn_summary::serialize")]
    turn_summary: Option<TurnSummary>,
}

#[derive(Serialize)]
struct StepWire<'a> {
    assistant: Option<&'a str>,
    provider_replay: Null,
    tool_calls: Vec<CallWire<'a>>,
    tool_results: Vec<ResultWire<'a>>,
}

#[derive(Serialize)]
struct CallWire<'a> {
    id: &'a str,
    name: &'a str,
    arguments_json: &'a str,
    provider_result: Null,
}

#[derive(Serialize)]
struct ResultWire<'a> {
    tool_call_id: &'a str,
    tool_name: &'a str,
    status: &'static str,
    output: &'a str,
    output_handle: Option<String>,
    preview: Option<&'a str>,
    output_bytes: u64,
    stored_output_bytes: u64,
    truncated: bool,
    provider_native: False,
    review_feedback: False,
    created_at_ms: i64,
    permission_feedback: &'a [String],
    committed_file_presentation: Null,
    command_output_replay: Null,
    #[serde(serialize_with = "process_presentation::checkpoint::serialize")]
    command_process_presentation: Option<CommandProcessPresentation>,
    terminal_action_presentation: Null,
}

#[derive(Serialize)]
struct SteeringWire<'a> {
    text: &'a str,
    assistant_prefix: Option<&'a str>,
    after_tool_step_count: usize,
}

#[derive(Serialize)]
struct AuthorityWire<'a> {
    provider: &'a SavedProvider,
    model: &'a str,
    credential_source: Option<&'static str>,
    credential_identity: Option<&'a str>,
}

pub(super) fn recovery_file(
    checkpoint: &Continuable,
    conversation_seq: u64,
) -> Result<Option<RecoveryFile>, SessionError> {
    let invalid = SessionError::InvalidRecoveryCheckpoint;
    let execution = &checkpoint.execution;
    if checkpoint.work_id.is_some() {
        return Err(invalid);
    }
    let mut spilled = Vec::new();
    let tool_steps = execution
        .steps
        .iter()
        .map(|step| step_wire(step, &mut spilled))
        .collect::<Option<Vec<_>>>()
        .ok_or(invalid)?;
    let ultrafast = checkpoint
        .ultrafast
        .filter(|(requested, effective)| *requested || *effective);
    let wire = CheckpointWire {
        version: CHECKPOINT_VERSION,
        turn_id: checkpoint.turn_id,
        user: UserWire {
            text: &checkpoint.user,
            images: NoItems,
        },
        assistant_source: &checkpoint.assistant_source,
        execution: ExecutionWire {
            schema_version: EXECUTION_SCHEMA_VERSION,
            tool_steps,
            files: execution
                .files
                .iter()
                .filter(|file| !file.path.is_empty())
                .collect(),
            steering: execution.steering.iter().map(steering_wire).collect(),
            turn_summary: execution.turn_summary,
        },
        cause: checkpoint.cause,
        action: checkpoint.action,
        tool_state: checkpoint.tool_state,
        authority: AuthorityWire {
            provider: &checkpoint.provider,
            model: &checkpoint.model,
            credential_source: checkpoint.credential_source,
            credential_identity: checkpoint.credential_identity.as_deref(),
        },
        requested_fast_mode: checkpoint.requested_fast_mode,
        fast_mode: checkpoint.fast_mode,
        requested_ultrafast_mode: ultrafast.map(|(requested, _)| requested),
        ultrafast_mode: ultrafast.map(|(_, effective)| effective),
        max_provider_attempts: checkpoint.max_provider_attempts,
        consumed_provider_attempts: checkpoint.consumed_provider_attempts,
        outstanding_reservation: checkpoint.outstanding_reservation,
    };
    let encoded = serde_json::to_vec(&wire).map_err(|_| invalid)?;
    if encoded.len() > EMERGENCY_CEILING_BYTES {
        return Ok(None);
    }
    let mut bytes =
        format!("{{\"conversation_seq\":{conversation_seq},\"checkpoint\":").into_bytes();
    bytes.extend_from_slice(&encoded);
    bytes.extend_from_slice(b"}\n");
    decode_recovery_file(&bytes, conversation_seq)?.ok_or(invalid)?;
    Ok(Some(RecoveryFile { bytes, spilled }))
}

fn step_wire<'a>(step: &'a Step, spilled: &mut Vec<(String, String)>) -> Option<StepWire<'a>> {
    let tool_calls = step
        .calls
        .iter()
        .map(|call| {
            call.provider_result.is_none().then_some(CallWire {
                id: &call.call_id,
                name: &call.tool_name,
                arguments_json: &call.arguments_json,
                provider_result: Null,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let tool_results = step
        .results
        .iter()
        .map(|result| result_wire(result, spilled))
        .collect::<Option<Vec<_>>>()?;
    Some(StepWire {
        assistant: (!step.assistant.is_empty()).then_some(step.assistant.as_str()),
        provider_replay: Null,
        tool_calls,
        tool_results,
    })
}

fn result_wire<'a>(
    result: &'a SavedResult,
    spilled: &mut Vec<(String, String)>,
) -> Option<ResultWire<'a>> {
    if result.provider_native {
        return None;
    }
    let mut output_handle = result.output_handle.clone();
    let mut stored_output_bytes = result.stored_output_bytes;
    let spills = output_handle.is_none()
        && result.output.len() > PREVIEW_BYTES
        && result.output.len() <= STORED_TEXT_MAX_BYTES;
    if spills {
        let handle = make_handle(&result.call_id, &result.tool_name, &result.output);
        spilled.push((handle.clone(), result.output.clone()));
        output_handle = Some(handle);
        stored_output_bytes = u64::try_from(result.output.len()).ok()?;
    }
    let moved = output_handle.is_some() && !result.output.is_empty();
    let preview = match (&result.preview, moved) {
        (Some(preview), _) => Some(preview.as_str()),
        (None, true) => Some(preview(&result.output)),
        (None, false) => None,
    };
    Some(ResultWire {
        tool_call_id: &result.call_id,
        tool_name: &result.tool_name,
        status: result.status.tag(),
        output: if moved { "" } else { &result.output },
        output_handle,
        preview,
        output_bytes: result.output_bytes,
        stored_output_bytes,
        truncated: result.truncated,
        provider_native: False,
        review_feedback: False,
        created_at_ms: result.created_at_ms,
        permission_feedback: &result.permission_feedback,
        committed_file_presentation: Null,
        command_output_replay: Null,
        command_process_presentation: result.process,
        terminal_action_presentation: Null,
    })
}

fn steering_wire(entry: &Steering) -> SteeringWire<'_> {
    SteeringWire {
        text: &entry.text,
        assistant_prefix: (!entry.assistant_prefix.is_empty())
            .then_some(entry.assistant_prefix.as_str()),
        after_tool_step_count: entry.after_tool_step_count,
    }
}
