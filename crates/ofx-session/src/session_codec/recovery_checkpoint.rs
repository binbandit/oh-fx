mod continuation;

use std::cmp::Ordering;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ofx_config::{EMERGENCY_CEILING_BYTES, PrivateDir};
use ofx_contract::{
    HistorySteering, HistoryStep, HistoryTurn, ProviderReplay, RecoveryStrategy, StepResult,
    ToolArgumentIntegrity, ToolCall, ToolResultStatus, TurnEnd, TurnStop,
};

use crate::fixed_field::{False, FixedField, NoItems, Null};
use crate::json_fields::{Fields, Json, parse_json};
use crate::result_store::{RESULT_UNAVAILABLE, format_stored_result_output, read_for_replay};
use crate::session_codec::{SavedProvider, parse_saved_provider};
use crate::session_error::SessionError;
use crate::session_event::{
    FileEvidence, KeptReplay, SavedReplay, WireTag, are_valid_files, saved_replay,
};

pub use continuation::CredentialAuthority;

pub(crate) const MAX_RECOVERY_FILE_BYTES: usize = EMERGENCY_CEILING_BYTES + 128;
const CHECKPOINT_VERSION: u64 = 2;
const EXECUTION_SCHEMA_VERSION: u64 = 10;
const CAUSES: [&str; 10] = [
    "network_interrupted",
    "connectivity_lost",
    "response_interrupted",
    "provider_stream_timeout",
    "provider_unavailable",
    "rate_limited",
    "system_resumed",
    "authentication",
    "request_limit_reached",
    "compaction_prepared",
];
const ACTIONS: [&str; 8] = [
    "retrying_request",
    "continuing_response",
    "regenerating_tool",
    "continuing_after_tool",
    "reconciling_tool",
    "waiting_for_connectivity",
    "checking_liveness",
    "paused",
];
const TOOL_STATES: [&str; 4] = ["none", "proven_unexecuted", "confirmed", "uncertain"];
const CREDENTIAL_SOURCES: [&str; 8] = [
    "vercel_oidc_token",
    "ai_gateway_api_key",
    "fx_login",
    "stored_key",
    "chatgpt_subscription",
    "grok_subscription",
    "host_managed",
    "configured",
];
const CREDENTIAL_IDENTITY_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryCheckpoint {
    user: String,
    assistant_source: String,
    execution: SavedExecution,
    strategy: RecoveryStrategy,
    route: RecoveryRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryRoute {
    provider: SavedProvider,
    model: String,
    credential: Option<SavedCredential>,
    requested_fast_mode: bool,
    fast_mode: bool,
    may_have_sent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedCredential {
    source: &'static str,
    identity: Option<[u8; CREDENTIAL_IDENTITY_BYTES]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedExecution {
    tool_steps: Vec<SavedToolStep>,
    files: Vec<FileEvidence>,
    steering: Vec<SavedSteering>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedToolStep {
    assistant: Option<String>,
    durable_replay: Option<SavedReplay>,
    provider_replay: Option<ProviderReplay>,
    tool_calls: Vec<ToolCall>,
    tool_results: Vec<SavedToolResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedToolResult {
    tool_call_id: String,
    tool_name: String,
    status: ToolResultStatus,
    output: String,
    output_handle: Option<String>,
    preview: Option<String>,
    output_bytes: usize,
    stored_output_bytes: u64,
    truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedSteering {
    text: String,
    assistant_prefix: Option<String>,
    after_tool_step_count: usize,
}

impl RecoveryCheckpoint {
    pub(crate) fn restore_outputs(&mut self, dir: &PrivateDir) {
        for step in &mut self.execution.tool_steps {
            for result in &mut step.tool_results {
                result.restore_output(dir);
            }
        }
    }

    pub(crate) fn interrupted_turn(&self) -> HistoryTurn<'_> {
        HistoryTurn {
            user: &self.user,
            steps: self
                .execution
                .tool_steps
                .iter()
                .map(SavedToolStep::history)
                .collect(),
            steering: self
                .execution
                .steering
                .iter()
                .map(|entry| HistorySteering {
                    text: &entry.text,
                    assistant_prefix: entry.assistant_prefix.as_deref().unwrap_or_default(),
                    after_tool_step_count: entry.after_tool_step_count,
                })
                .collect(),
            end: TurnEnd::Stopped {
                reason: TurnStop::Failed,
                partial: &self.assistant_source,
            },
        }
    }

    pub(crate) fn positional_replays(&self) -> Vec<Option<SavedReplay>> {
        self.execution
            .tool_steps
            .iter()
            .map(|step| step.durable_replay.clone())
            .collect()
    }

    pub(crate) fn saved_replays(&self) -> Vec<KeptReplay> {
        self.execution
            .tool_steps
            .iter()
            .filter_map(|step| {
                Some(KeptReplay {
                    call_ids: step
                        .tool_calls
                        .iter()
                        .map(|call| call.id.as_str().to_owned())
                        .collect(),
                    assistant: step.assistant.clone().unwrap_or_default(),
                    replay: step.durable_replay.clone()?,
                })
            })
            .collect()
    }

    pub(crate) fn into_files(self) -> Vec<FileEvidence> {
        self.execution.files
    }
}

impl SavedToolStep {
    fn history(&self) -> HistoryStep<'_> {
        HistoryStep {
            assistant: self.assistant.as_deref().unwrap_or_default(),
            provider_replay: self.provider_replay.as_ref(),
            tool_calls: &self.tool_calls,
            tool_results: self
                .tool_results
                .iter()
                .map(|result| StepResult {
                    call_id: &result.tool_call_id,
                    tool_name: &result.tool_name,
                    output: &result.output,
                    output_bytes: result.output_bytes,
                    status: result.status,
                })
                .collect(),
        }
    }
}

impl SavedToolResult {
    fn restore_output(&mut self, dir: &PrivateDir) {
        let Some(handle) = &self.output_handle else {
            return;
        };
        let inline = u64::try_from(self.output.len()).ok() == Some(self.stored_output_bytes);
        if !self.truncated && inline {
            return;
        }
        self.output = if self.truncated {
            format_stored_result_output(
                handle,
                self.preview.as_deref().unwrap_or_default(),
                self.stored_output_bytes,
            )
        } else {
            read_for_replay(dir, handle, self.stored_output_bytes)
                .unwrap_or_else(|| RESULT_UNAVAILABLE.to_owned())
        };
    }
}

pub(crate) fn decode_recovery_file(
    bytes: &[u8],
    conversation_seq: u64,
) -> Result<Option<RecoveryCheckpoint>, SessionError> {
    let invalid = SessionError::InvalidRecoveryCheckpoint;
    let mut file = parse_json(bytes)
        .ok()
        .and_then(Fields::new)
        .ok_or(invalid)?;
    let saved_seq = file.unsigned("conversation_seq").ok_or(invalid)?;
    let checkpoint = file.required("checkpoint").ok_or(invalid)?;
    file.finish(()).ok_or(invalid)?;
    match saved_seq.cmp(&conversation_seq) {
        Ordering::Less => Ok(None),
        Ordering::Greater => Err(invalid),
        Ordering::Equal => checkpoint_from(checkpoint).map(Some).ok_or(invalid),
    }
}

fn checkpoint_from(value: Json<'_>) -> Option<RecoveryCheckpoint> {
    let mut fields = Fields::new(value)?;
    fields
        .unsigned("version")
        .filter(|version| *version == CHECKPOINT_VERSION)?;
    fields.unsigned("turn_id")?;
    let user = user_text(fields.required("user")?)?;
    let assistant_source = durable_text(fields.required("assistant_source")?)?;
    let execution = execution(fields.required("execution")?)?;
    let cause = one_of(&fields.required("cause")?, &CAUSES)?;
    one_of(&fields.required("action")?, &ACTIONS)?;
    let tool_state = one_of(&fields.required("tool_state")?, &TOOL_STATES)?;
    let (provider, model, credential) = authority(fields.required("authority")?)?;
    let requested_fast_mode = fields.flag("requested_fast_mode")?;
    let fast_mode = fields.flag("fast_mode")?;
    fields.unsigned("max_provider_attempts")?;
    let consumed_attempts = fields.unsigned("consumed_provider_attempts")?;
    let outstanding_reservation = fields.flag("outstanding_reservation")?;
    let checkpoint = RecoveryCheckpoint {
        strategy: strategy(cause, tool_state, &assistant_source),
        user,
        assistant_source,
        execution,
        route: RecoveryRoute {
            provider,
            model,
            credential,
            requested_fast_mode,
            fast_mode,
            may_have_sent: consumed_attempts > 0 || outstanding_reservation,
        },
    };
    fields.finish(checkpoint)
}

fn strategy(cause: &str, tool_state: &str, assistant_source: &str) -> RecoveryStrategy {
    match tool_state {
        _ if cause == "request_limit_reached" => RecoveryStrategy::RetryRequest,
        "proven_unexecuted" => RecoveryStrategy::RegenerateTool,
        "confirmed" => RecoveryStrategy::ContinueAfterTool,
        "uncertain" => RecoveryStrategy::ReconcileTool,
        _ if assistant_source.is_empty() => RecoveryStrategy::RetryRequest,
        _ => RecoveryStrategy::ContinueResponse,
    }
}

fn user_text(value: Json<'_>) -> Option<String> {
    let mut fields = Fields::new(value)?;
    let text = durable_text(fields.required("text")?).filter(|text| !text.is_empty())?;
    fixed::<NoItems>(&mut fields, "images")?;
    fields.finish(text)
}

fn execution(value: Json<'_>) -> Option<SavedExecution> {
    let mut fields = Fields::new(value)?;
    fields
        .unsigned("schema_version")
        .filter(|version| *version == EXECUTION_SCHEMA_VERSION)?;
    let tool_steps = list(fields.required("tool_steps")?, tool_step)?;
    let files =
        list(fields.required("files")?, file_evidence).filter(|files| are_valid_files(files))?;
    let steering = list(fields.required("steering")?, steering_entry)?;
    fixed::<Null>(&mut fields, "turn_summary")?;
    let ordered = steering
        .windows(2)
        .all(|pair| pair[0].after_tool_step_count <= pair[1].after_tool_step_count);
    let bounded = steering
        .last()
        .is_none_or(|entry| entry.after_tool_step_count <= tool_steps.len());
    if !ordered || !bounded {
        return None;
    }
    fields.finish(SavedExecution {
        tool_steps,
        files,
        steering,
    })
}

fn tool_step(value: Json<'_>) -> Option<SavedToolStep> {
    let mut fields = Fields::new(value)?;
    let assistant = fields.present_or_null("assistant", |value| durable_text(value).map(Some))?;
    let durable_replay =
        fields.present_or_null("provider_replay", |value| saved_replay(value).map(Some))?;
    let step = SavedToolStep {
        assistant,
        provider_replay: durable_replay
            .clone()
            .map(SavedReplay::into_provider_replay),
        durable_replay,
        tool_calls: list(fields.required("tool_calls")?, tool_call)?,
        tool_results: list(fields.required("tool_results")?, tool_result)?,
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

fn tool_call(value: Json<'_>) -> Option<ToolCall> {
    let mut fields = Fields::new(value)?;
    let id = durable_text(fields.required("id")?)?;
    let name = durable_text(fields.required("name")?)?;
    let mut arguments = durable_text(fields.required("arguments_json")?)?;
    fixed::<Null>(&mut fields, "provider_result")?;
    if ToolArgumentIntegrity::classify_function_input(&arguments)
        == ToolArgumentIntegrity::MalformedJson
    {
        "{}".clone_into(&mut arguments);
    }
    fields.finish(ToolCall::new(id, name, arguments))
}

fn tool_result(value: Json<'_>) -> Option<SavedToolResult> {
    let mut fields = Fields::new(value)?;
    let result = SavedToolResult {
        tool_call_id: durable_text(fields.required("tool_call_id")?)?,
        tool_name: durable_text(fields.required("tool_name")?)?,
        status: tag(&fields.required("status")?)?,
        output: durable_text(fields.required("output")?)?,
        output_handle: fields
            .present_or_null("output_handle", |value| durable_text(value).map(Some))?,
        preview: fields.present_or_null("preview", |value| durable_text(value).map(Some))?,
        output_bytes: usize::try_from(fields.unsigned("output_bytes")?).ok()?,
        stored_output_bytes: fields.unsigned("stored_output_bytes")?,
        truncated: fields.flag("truncated")?,
    };
    fixed::<False>(&mut fields, "provider_native")?;
    fixed::<False>(&mut fields, "review_feedback")?;
    fields.signed("created_at_ms")?;
    fixed::<NoItems>(&mut fields, "permission_feedback")?;
    for presentation in [
        "committed_file_presentation",
        "command_output_replay",
        "command_process_presentation",
        "terminal_action_presentation",
    ] {
        fixed::<Null>(&mut fields, presentation)?;
    }
    fields.finish(result)
}

fn file_evidence(value: Json<'_>) -> Option<FileEvidence> {
    let mut fields = Fields::new(value)?;
    let file = FileEvidence {
        path: durable_text(fields.required("path")?)?,
        new_path: fields.present_or_null("new_path", |value| durable_text(value).map(Some))?,
        tool_call_id: durable_text(fields.required("tool_call_id")?)?,
        tool_name: durable_text(fields.required("tool_name")?)?,
        action: tag(&fields.required("action")?)?,
        status: tag(&fields.required("status")?)?,
        model_view_covers_full_file: fields.flag("model_view_covers_full_file")?,
        stale: fields.flag("stale")?,
    };
    fields.finish(file)
}

fn steering_entry(value: Json<'_>) -> Option<SavedSteering> {
    let mut fields = Fields::new(value)?;
    let entry = SavedSteering {
        text: durable_text(fields.required("text")?)?,
        assistant_prefix: fields
            .present_or_null("assistant_prefix", |value| durable_text(value).map(Some))?,
        after_tool_step_count: usize::try_from(fields.unsigned("after_tool_step_count")?).ok()?,
    };
    fields.finish(entry)
}

fn authority(value: Json<'_>) -> Option<(SavedProvider, String, Option<SavedCredential>)> {
    let mut fields = Fields::new(value)?;
    let provider = parse_saved_provider(&fields.required("provider")?)?;
    let model = durable_text(fields.required("model")?)?;
    let source = match fields.required("credential_source")? {
        Json::Null => None,
        source => Some(one_of(&source, &CREDENTIAL_SOURCES)?),
    };
    let identity = match fields.required("credential_identity")? {
        Json::Null => None,
        Json::String(hex) => Some(lowercase_digest(&hex)?),
        _ => return None,
    };
    let credential = match (source, identity) {
        (None, Some(_)) => return None,
        (None, None) => None,
        (Some(source), identity) => Some(SavedCredential { source, identity }),
    };
    fields.finish((provider, model, credential))
}

fn lowercase_digest(hex: &str) -> Option<[u8; CREDENTIAL_IDENTITY_BYTES]> {
    let lowercase = hex
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !lowercase || hex.len() != CREDENTIAL_IDENTITY_BYTES * 2 {
        return None;
    }
    let mut digest = [0_u8; CREDENTIAL_IDENTITY_BYTES];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

fn durable_text(value: Json<'_>) -> Option<String> {
    match value {
        Json::String(text) => Some(text.into_owned()),
        Json::Object(entries) => match entries.entries() {
            [
                (encoding, Json::String(scheme)),
                (data, Json::String(encoded)),
            ] if encoding == "encoding" && scheme == "base64" && data == "data" => {
                String::from_utf8(STANDARD.decode(encoded.as_bytes()).ok()?).ok()
            }
            _ => None,
        },
        _ => None,
    }
}

fn list<T>(value: Json<'_>, item: impl Fn(Json<'_>) -> Option<T>) -> Option<Vec<T>> {
    match value {
        Json::Array(items) => items.into_iter().map(item).collect(),
        _ => None,
    }
}

fn fixed<T: FixedField>(fields: &mut Fields<'_>, key: &str) -> Option<()> {
    T::accepts(&fields.required(key)?).then_some(())
}

fn one_of(value: &Json<'_>, tags: &[&'static str]) -> Option<&'static str> {
    let text = value.as_str()?;
    tags.iter().copied().find(|tag| *tag == text)
}

fn tag<T: WireTag>(value: &Json<'_>) -> Option<T> {
    T::from_tag(value.as_str()?)
}

#[cfg(test)]
mod tests;
