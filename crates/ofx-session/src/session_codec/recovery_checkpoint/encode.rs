use ofx_config::EMERGENCY_CEILING_BYTES;
use ofx_contract::{CommandProcessPresentation, HistoryStep, RecoveryPoint, RecoveryProgress};
use serde::Serialize;

use super::{CHECKPOINT_VERSION, EXECUTION_SCHEMA_VERSION, RouteCredential};
use crate::fixed_field::{False, NoItems, Null};
use crate::process_presentation;
use crate::session_codec::SavedProvider;
use crate::session_error::SessionError;
use crate::session_event::{FileEvidence, SavedReplay, WireTag};

pub(crate) struct CheckpointSource<'a> {
    pub(crate) point: &'a RecoveryPoint<'a>,
    pub(crate) provider: &'a SavedProvider,
    pub(crate) credential: Option<RouteCredential>,
    pub(crate) replays: Vec<Option<SavedReplay>>,
    pub(crate) outputs: Vec<Vec<SavedOutput>>,
    pub(crate) files: Vec<FileEvidence>,
    pub(crate) created_at_ms: i64,
}

pub(crate) struct SavedOutput {
    pub(crate) handle: Option<String>,
    pub(crate) preview: Option<String>,
}

#[derive(Serialize)]
struct FileWire<'a> {
    conversation_seq: u64,
    checkpoint: CheckpointWire<'a>,
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
    max_provider_attempts: usize,
    consumed_provider_attempts: usize,
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
    files: &'a [FileEvidence],
    steering: Vec<SteeringWire<'a>>,
    turn_summary: Null,
}

#[derive(Serialize)]
struct StepWire<'a> {
    assistant: Option<&'a str>,
    provider_replay: Option<&'a SavedReplay>,
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
    output_handle: Option<&'a str>,
    preview: Option<&'a str>,
    output_bytes: usize,
    stored_output_bytes: usize,
    truncated: bool,
    provider_native: False,
    review_feedback: False,
    created_at_ms: i64,
    permission_feedback: &'a [&'a str],
    committed_file_presentation: Null,
    command_output_replay: Null,
    #[serde(with = "process_presentation::checkpoint")]
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
    credential_identity: Option<String>,
}

pub(crate) fn encode_recovery_file(
    conversation_seq: u64,
    source: &CheckpointSource<'_>,
) -> Result<Option<Vec<u8>>, SessionError> {
    let point = source.point;
    let checkpoint = CheckpointWire {
        version: CHECKPOINT_VERSION,
        turn_id: point.turn_id.get(),
        user: UserWire {
            text: point.turn.user,
            images: NoItems,
        },
        assistant_source: point.source,
        execution: ExecutionWire {
            schema_version: EXECUTION_SCHEMA_VERSION,
            tool_steps: point
                .turn
                .steps
                .iter()
                .enumerate()
                .map(|(index, step)| step_wire(source, index, step))
                .collect(),
            files: &source.files,
            steering: point
                .turn
                .steering
                .iter()
                .map(|entry| SteeringWire {
                    text: entry.text,
                    assistant_prefix: Some(entry.assistant_prefix).filter(|text| !text.is_empty()),
                    after_tool_step_count: entry.after_tool_step_count,
                })
                .collect(),
            turn_summary: Null,
        },
        cause: point.cause.as_str(),
        action: match point.progress {
            RecoveryProgress::Waiting(action) => action.as_str(),
            RecoveryProgress::Paused => "paused",
        },
        tool_state: point.tool_state.as_str(),
        authority: AuthorityWire {
            provider: source.provider,
            model: point.model,
            credential_source: source.credential.map(RouteCredential::source),
            credential_identity: source.credential.and_then(RouteCredential::identity_hex),
        },
        requested_fast_mode: point.requested_fast_mode,
        fast_mode: point.fast_mode,
        requested_ultrafast_mode: point.ultrafast_mode.then_some(true),
        ultrafast_mode: point.ultrafast_mode.then_some(true),
        max_provider_attempts: point.attempt_limit,
        consumed_provider_attempts: point.consumed_attempts,
        outstanding_reservation: false,
    };
    let encoded =
        serde_json::to_vec(&checkpoint).map_err(|_| SessionError::InvalidConversationEvent)?;
    if encoded.len() > EMERGENCY_CEILING_BYTES {
        return Ok(None);
    }
    let mut file = serde_json::to_vec(&FileWire {
        conversation_seq,
        checkpoint,
    })
    .map_err(|_| SessionError::InvalidConversationEvent)?;
    file.push(b'\n');
    Ok(Some(file))
}

fn step_wire<'a>(
    source: &'a CheckpointSource<'_>,
    index: usize,
    step: &'a HistoryStep<'_>,
) -> StepWire<'a> {
    let outputs = source.outputs.get(index).map_or(&[][..], Vec::as_slice);
    StepWire {
        assistant: Some(step.assistant).filter(|text| !text.is_empty()),
        provider_replay: source.replays.get(index).and_then(Option::as_ref),
        tool_calls: step
            .tool_calls
            .iter()
            .map(|call| CallWire {
                id: call.id.as_str(),
                name: &call.name,
                arguments_json: &call.arguments,
                provider_result: Null,
            })
            .collect(),
        tool_results: step
            .tool_results
            .iter()
            .enumerate()
            .map(|(position, result)| {
                let saved = outputs.get(position);
                let handle = saved.and_then(|saved| saved.handle.as_deref());
                ResultWire {
                    tool_call_id: result.call_id,
                    tool_name: result.tool_name,
                    status: result.status.tag(),
                    output: if handle.is_some() { "" } else { result.output },
                    output_handle: handle,
                    preview: saved.and_then(|saved| saved.preview.as_deref()),
                    output_bytes: result.output_bytes,
                    stored_output_bytes: result.output.len(),
                    truncated: false,
                    provider_native: False,
                    review_feedback: False,
                    created_at_ms: source.created_at_ms,
                    permission_feedback: &result.permission_feedback,
                    committed_file_presentation: Null,
                    command_output_replay: Null,
                    command_process_presentation: result.process,
                    terminal_action_presentation: Null,
                }
            })
            .collect(),
    }
}
