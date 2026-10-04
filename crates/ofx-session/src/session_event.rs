mod frame_decode;
mod history_codec;

use ofx_config::EMERGENCY_CEILING_BYTES;
pub(crate) use ofx_contract::FileEvidenceAction;
use ofx_contract::{
    CommandProcessPresentation, ProviderReplay, ReplaySource, ToolArgumentIntegrity,
    ToolExecutionProvenance, ToolResultStatus, TurnSummary,
};
use serde::Serialize;

use crate::fixed_field::{False, NoItems, Null, TurnOrigin, ValidIdentity};
use crate::json_fields::parse_json;
use crate::session_codec::SavedProvider;
use crate::session_error::SessionError;
use crate::session_store_paths::MAX_PATH_BYTES;
use frame_decode::envelope_from;
pub(crate) use frame_decode::saved_replay;
pub(crate) use history_codec::{decode_history_envelope, encode_history_envelope};

pub(crate) const CONVERSATION_SCHEMA_VERSION: u8 = 3;
pub(crate) const EVENT_FRAME_MAX_BYTES: usize = EMERGENCY_CEILING_BYTES;
const MAX_TEXT_BYTES: usize = EVENT_FRAME_MAX_BYTES;
const MAX_IDENTITY_BYTES: usize = 256;
const MAX_PREVIEW_BYTES: usize = 4 * 1024;
const MAX_REPLAY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(rename_all = "snake_case")]
pub enum ConversationEvent {
    User(UserEvent),
    Assistant(AssistantEvent),
    ToolCall(ToolCallEvent),
    ToolResult(ToolResultEvent),
    Steering(SteeringEvent),
    TurnCompleted(TurnCompletedEvent),
    Interrupted(InterruptedEvent),
    ContextCheckpoint(ContextCheckpointEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct UserEvent {
    pub text: String,
    #[serde(default)]
    images: NoItems,
    #[serde(default)]
    pub work_id: Option<String>,
}

impl UserEvent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: NoItems,
            work_id: None,
        }
    }

    pub fn for_work(text: impl Into<String>, work_id: impl Into<String>) -> Self {
        Self {
            work_id: Some(work_id.into()),
            ..Self::new(text)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct AssistantEvent {
    pub text: String,
    #[serde(default)]
    pub provider_replay: Option<SavedReplay>,
    #[serde(default)]
    pub standalone_response: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct SavedReplay {
    pub source: SavedReplaySource,
    pub parts_json: String,
}

impl SavedReplay {
    pub(crate) fn into_provider_replay(self) -> ProviderReplay {
        ProviderReplay {
            source: ReplaySource {
                provider: self.source.provider.id().label().to_owned(),
                model: self.source.model,
                binding: self.source.provider.binding(),
            },
            parts_json: self.parts_json,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct SavedReplaySource {
    pub provider: SavedProvider,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct ToolCallEvent {
    pub call_id: String,
    pub tool_name: String,
    pub arguments_json: String,
    #[serde(default = "valid_arguments", with = "wire_tag")]
    pub argument_integrity: ToolArgumentIntegrity,
    #[serde(default)]
    provisional_id: Null,
    #[serde(default)]
    pub provider_result: Option<String>,
    #[serde(default)]
    final_identity: ValidIdentity,
    #[serde(default, with = "wire_tag")]
    pub provenance: ToolExecutionProvenance,
}

impl ToolCallEvent {
    pub fn new(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments_json: impl Into<String>,
        argument_integrity: ToolArgumentIntegrity,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            arguments_json: arguments_json.into(),
            argument_integrity,
            provisional_id: Null,
            provider_result: None,
            final_identity: ValidIdentity,
            provenance: ToolExecutionProvenance::FxLocal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(rename_all = "snake_case")]
pub enum ArtifactCompleteness {
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct ToolResultEvent {
    pub call_id: String,
    pub tool_name: String,
    #[serde(with = "wire_tag")]
    pub status: ToolResultStatus,
    pub artifact_ref: String,
    #[serde(default)]
    tool_image_handle: Null,
    #[serde(default)]
    pub output_bytes: Option<u64>,
    pub stored_bytes: u64,
    pub completeness: ArtifactCompleteness,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default)]
    provider_native: bool,
    #[serde(default, skip_serializing)]
    review_feedback: False,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    permission_feedback: NoItems,
    #[serde(default)]
    committed_file_presentation: Null,
    #[serde(default)]
    command_replay_ref: Null,
    #[serde(default)]
    command_replay_bytes: Null,
    #[serde(default, with = "crate::process_presentation::frame")]
    pub command_process_presentation: Option<CommandProcessPresentation>,
    #[serde(default)]
    terminal_action_presentation: Null,
}

impl ToolResultEvent {
    pub fn new(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        status: ToolResultStatus,
        artifact_ref: impl Into<String>,
        stored_bytes: u64,
        completeness: ArtifactCompleteness,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            status,
            artifact_ref: artifact_ref.into(),
            tool_image_handle: Null,
            output_bytes: None,
            stored_bytes,
            completeness,
            preview: None,
            provider_native: false,
            review_feedback: False,
            created_at_ms: 0,
            permission_feedback: NoItems,
            committed_file_presentation: Null,
            command_replay_ref: Null,
            command_replay_bytes: Null,
            command_process_presentation: None,
            terminal_action_presentation: Null,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct SteeringEvent {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct FileEvidence {
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) new_path: Option<String>,
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    #[serde(default, with = "wire_tag")]
    pub(crate) action: FileEvidenceAction,
    #[serde(with = "wire_tag")]
    #[cfg_attr(test, serde(default = "succeeded"))]
    pub(crate) status: ToolResultStatus,
    #[serde(default)]
    pub(crate) model_view_covers_full_file: bool,
    #[serde(default)]
    pub(crate) stale: bool,
}

impl From<&ofx_contract::FileEvidence> for FileEvidence {
    fn from(file: &ofx_contract::FileEvidence) -> Self {
        Self {
            path: file.path.clone(),
            new_path: file.new_path.clone(),
            tool_call_id: file.tool_call_id.clone(),
            tool_name: file.tool_name.clone(),
            action: file.action,
            status: file.status,
            model_view_covers_full_file: file.model_view_covers_full_file,
            stale: file.stale,
        }
    }
}

impl From<FileEvidence> for ofx_contract::FileEvidence {
    fn from(file: FileEvidence) -> Self {
        Self {
            path: file.path,
            new_path: file.new_path,
            tool_call_id: file.tool_call_id,
            tool_name: file.tool_name,
            action: file.action,
            status: file.status,
            model_view_covers_full_file: file.model_view_covers_full_file,
            stale: file.stale,
        }
    }
}

#[cfg(test)]
fn succeeded() -> ToolResultStatus {
    ToolResultStatus::Success
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct TurnCompletedEvent {
    #[serde(default)]
    pub(crate) files: Vec<FileEvidence>,
    #[serde(default, with = "crate::turn_summary")]
    pub turn_summary: Option<TurnSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(rename_all = "snake_case")]
pub enum InterruptReason {
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct InterruptedEvent {
    pub reason: InterruptReason,
    #[serde(default)]
    pub partial_text: Option<String>,
    #[serde(default)]
    command_replay_ref: Null,
    #[serde(default)]
    command_replay_bytes: Null,
    #[serde(default)]
    command_artifact_ref: Null,
    #[serde(default)]
    pub(crate) files: Vec<FileEvidence>,
    #[serde(default, with = "crate::turn_summary")]
    pub turn_summary: Option<TurnSummary>,
    #[serde(default, skip_serializing)]
    cancellation_origin: TurnOrigin,
}

impl InterruptedEvent {
    pub fn new(reason: InterruptReason, partial_text: Option<String>) -> Self {
        Self {
            reason,
            partial_text,
            command_replay_ref: Null,
            command_replay_bytes: Null,
            command_artifact_ref: Null,
            files: Vec::new(),
            turn_summary: None,
            cancellation_origin: TurnOrigin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct ContextCheckpointEvent {
    pub covers_through_seq: u64,
    pub summary: String,
}

#[derive(Serialize)]
struct EnvelopeWire<'a> {
    schema_version: u8,
    seq: u64,
    timestamp_ms: i64,
    event: &'a ConversationEvent,
}

#[cfg_attr(test, derive(serde::Deserialize), serde(deny_unknown_fields))]
pub(crate) struct ConversationEnvelope {
    #[cfg_attr(test, serde(default = "current_schema_version"))]
    schema_version: u8,
    pub(crate) seq: u64,
    timestamp_ms: i64,
    pub(crate) event: ConversationEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingToolCall {
    call_id: String,
    tool_name: String,
    seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnsweredToolSpan {
    first_call_seq: u64,
    last_result_seq: u64,
}

impl AnsweredToolSpan {
    fn splits_at(self, covers_through_seq: u64) -> bool {
        self.first_call_seq <= covers_through_seq && covers_through_seq < self.last_result_seq
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConversationState {
    last_seq: u64,
    latest_checkpoint_coverage: u64,
    pending_tool_calls: Vec<PendingToolCall>,
    answered_tool_spans: Vec<AnsweredToolSpan>,
    turn_open: bool,
}

impl ConversationState {
    pub(crate) fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub(crate) fn turn_open(&self) -> bool {
        self.turn_open
    }

    pub(crate) fn latest_checkpoint_coverage(&self) -> u64 {
        self.latest_checkpoint_coverage
    }

    pub(crate) fn has_pending_tool_calls(&self) -> bool {
        !self.pending_tool_calls.is_empty()
    }

    pub(crate) fn next_seq(&self) -> Result<u64, SessionError> {
        self.last_seq
            .checked_add(1)
            .ok_or(SessionError::ConversationSequenceOverflow)
    }

    pub(crate) fn apply(
        &mut self,
        seq: u64,
        timestamp_ms: i64,
        event: &ConversationEvent,
    ) -> Result<(), SessionError> {
        self.validate(seq, timestamp_ms, event)?;
        let turn_open = self.next_turn_open(event)?;
        match event {
            ConversationEvent::ToolCall(call) => self.pending_tool_calls.push(PendingToolCall {
                call_id: call.call_id.clone(),
                tool_name: call.tool_name.clone(),
                seq,
            }),
            ConversationEvent::ToolResult(result) => self.answer(&result.call_id, seq),
            ConversationEvent::Interrupted(_) => self.pending_tool_calls.clear(),
            ConversationEvent::ContextCheckpoint(checkpoint) => {
                let coverage = checkpoint.covers_through_seq;
                self.latest_checkpoint_coverage = coverage;
                self.answered_tool_spans
                    .retain(|span| span.last_result_seq > coverage);
            }
            ConversationEvent::User(_)
            | ConversationEvent::Assistant(_)
            | ConversationEvent::Steering(_)
            | ConversationEvent::TurnCompleted(_) => {}
        }
        self.last_seq = seq;
        self.turn_open = turn_open;
        Ok(())
    }

    pub(crate) fn rewind_open_turn(&mut self, last_seq: u64, keeps_checkpoint: bool) {
        self.last_seq = last_seq;
        self.pending_tool_calls.clear();
        self.answered_tool_spans
            .retain(|span| span.last_result_seq <= last_seq);
        self.turn_open = keeps_checkpoint;
    }

    fn validate(
        &self,
        seq: u64,
        timestamp_ms: i64,
        event: &ConversationEvent,
    ) -> Result<(), SessionError> {
        if self.last_seq.checked_add(1) != Some(seq) {
            return Err(SessionError::OutOfOrderConversationEvent);
        }
        if timestamp_ms < 0 {
            return Err(SessionError::InvalidConversationEvent);
        }
        validate_event_shape(event)?;
        match event {
            ConversationEvent::ToolCall(call) => {
                if self.pending(&call.call_id).is_some() {
                    return Err(SessionError::DuplicateToolCall);
                }
            }
            ConversationEvent::ToolResult(result) => {
                let pending = self
                    .pending(&result.call_id)
                    .ok_or(SessionError::OrphanToolResult)?;
                if pending.tool_name != result.tool_name {
                    return Err(SessionError::ToolIdentityMismatch);
                }
            }
            ConversationEvent::ContextCheckpoint(checkpoint) => {
                if checkpoint.covers_through_seq < self.latest_checkpoint_coverage
                    || checkpoint.covers_through_seq > self.last_seq
                {
                    return Err(SessionError::InvalidCheckpointCoverage);
                }
                if self
                    .pending_tool_calls
                    .iter()
                    .any(|pending| pending.seq <= checkpoint.covers_through_seq)
                {
                    return Err(SessionError::UnresolvedToolCall);
                }
                if self
                    .answered_tool_spans
                    .iter()
                    .any(|span| span.splits_at(checkpoint.covers_through_seq))
                {
                    return Err(SessionError::InvalidCheckpointCoverage);
                }
            }
            ConversationEvent::TurnCompleted(_) => {
                if self.has_pending_tool_calls() {
                    return Err(SessionError::UnresolvedToolCall);
                }
            }
            ConversationEvent::User(_)
            | ConversationEvent::Assistant(_)
            | ConversationEvent::Steering(_)
            | ConversationEvent::Interrupted(_) => {}
        }
        Ok(())
    }

    fn next_turn_open(&self, event: &ConversationEvent) -> Result<bool, SessionError> {
        let open = self.turn_open;
        match event {
            ConversationEvent::User(_) if !open => Ok(true),
            ConversationEvent::TurnCompleted(_) | ConversationEvent::Interrupted(_) if open => {
                Ok(false)
            }
            ConversationEvent::Assistant(_)
            | ConversationEvent::ToolCall(_)
            | ConversationEvent::ToolResult(_)
            | ConversationEvent::Steering(_)
                if open =>
            {
                Ok(true)
            }
            ConversationEvent::ContextCheckpoint(_) => Ok(open),
            _ => Err(SessionError::InvalidConversationFrame),
        }
    }

    fn answer(&mut self, call_id: &str, result_seq: u64) {
        let Some(index) = self
            .pending_tool_calls
            .iter()
            .position(|pending| pending.call_id == call_id)
        else {
            return;
        };
        let call_seq = self.pending_tool_calls.remove(index).seq;
        match self.answered_tool_spans.last_mut() {
            Some(span) if call_seq <= span.last_result_seq => {
                span.first_call_seq = span.first_call_seq.min(call_seq);
                span.last_result_seq = result_seq;
            }
            _ => self.answered_tool_spans.push(AnsweredToolSpan {
                first_call_seq: call_seq,
                last_result_seq: result_seq,
            }),
        }
    }

    fn pending(&self, call_id: &str) -> Option<&PendingToolCall> {
        self.pending_tool_calls
            .iter()
            .find(|pending| pending.call_id == call_id)
    }
}

pub(crate) fn encode_conversation_frame(
    seq: u64,
    timestamp_ms: i64,
    event: &ConversationEvent,
) -> Result<Vec<u8>, SessionError> {
    if seq == 0 || timestamp_ms < 0 {
        return Err(SessionError::InvalidConversationEvent);
    }
    validate_event_shape(event)?;
    let mut frame = serde_json::to_vec(&EnvelopeWire {
        schema_version: CONVERSATION_SCHEMA_VERSION,
        seq,
        timestamp_ms,
        event,
    })
    .map_err(|_| SessionError::InvalidConversationEvent)?;
    frame.push(b'\n');
    if frame.len() > EVENT_FRAME_MAX_BYTES {
        return Err(SessionError::EventFrameTooLarge);
    }
    Ok(frame)
}

pub(crate) fn decode_conversation_frame(
    bytes: &[u8],
) -> Result<ConversationEnvelope, SessionError> {
    if bytes.is_empty() || bytes.len() > EVENT_FRAME_MAX_BYTES || bytes.last() != Some(&b'\n') {
        return Err(SessionError::InvalidConversationFrame);
    }
    let envelope = parse_json(bytes)
        .ok()
        .and_then(envelope_from)
        .ok_or(SessionError::InvalidConversationFrame)?;
    if envelope.schema_version != CONVERSATION_SCHEMA_VERSION
        || envelope.seq == 0
        || envelope.timestamp_ms < 0
        || validate_event_shape(&envelope.event).is_err()
    {
        return Err(SessionError::InvalidConversationFrame);
    }
    Ok(envelope)
}

impl ConversationEnvelope {
    pub(crate) fn timestamp_ms(&self) -> i64 {
        self.timestamp_ms
    }
}

fn validate_event_shape(event: &ConversationEvent) -> Result<(), SessionError> {
    let valid = match event {
        ConversationEvent::User(user) => {
            is_valid_text(&user.text) && user.work_id.as_deref().is_none_or(is_valid_identity)
        }
        ConversationEvent::Assistant(assistant) => {
            assistant.text.len() <= MAX_TEXT_BYTES
                && assistant.provider_replay.as_ref().is_none_or(|replay| {
                    is_valid_identity(&replay.source.model)
                        && (1..=MAX_REPLAY_BYTES).contains(&replay.parts_json.len())
                })
        }
        ConversationEvent::Steering(steering) => is_valid_text(&steering.text),
        ConversationEvent::ToolCall(call) => {
            is_valid_identity(&call.call_id)
                && is_valid_identity(&call.tool_name)
                && (1..=MAX_TEXT_BYTES).contains(&call.arguments_json.len())
                && call
                    .provider_result
                    .as_ref()
                    .is_none_or(|result| result.len() <= MAX_TEXT_BYTES)
        }
        ConversationEvent::ToolResult(result) => {
            is_valid_identity(&result.call_id)
                && is_valid_identity(&result.tool_name)
                && is_valid_identity(&result.artifact_ref)
                && result
                    .preview
                    .as_ref()
                    .is_none_or(|preview| preview.len() <= MAX_PREVIEW_BYTES)
                && result.created_at_ms >= 0
        }
        ConversationEvent::Interrupted(interrupted) => {
            interrupted
                .partial_text
                .as_ref()
                .is_none_or(|text| text.len() <= MAX_TEXT_BYTES)
                && are_valid_files(&interrupted.files)
        }
        ConversationEvent::ContextCheckpoint(checkpoint) => is_valid_text(&checkpoint.summary),
        ConversationEvent::TurnCompleted(completed) => are_valid_files(&completed.files),
    };
    if valid {
        Ok(())
    } else {
        Err(SessionError::InvalidConversationEvent)
    }
}

pub(crate) fn are_valid_files(files: &[FileEvidence]) -> bool {
    files.iter().all(|file| {
        is_valid_path(&file.path)
            && file.new_path.as_deref().is_none_or(is_valid_path)
            && is_valid_identity(&file.tool_call_id)
            && is_valid_identity(&file.tool_name)
    })
}

fn is_valid_path(path: &str) -> bool {
    (1..=MAX_PATH_BYTES).contains(&path.len())
}

fn is_valid_text(text: &str) -> bool {
    (1..=MAX_TEXT_BYTES).contains(&text.len())
}

fn is_valid_identity(value: &str) -> bool {
    (1..=MAX_IDENTITY_BYTES).contains(&value.len())
}

#[cfg(test)]
fn current_schema_version() -> u8 {
    CONVERSATION_SCHEMA_VERSION
}

#[cfg(test)]
fn valid_arguments() -> ToolArgumentIntegrity {
    ToolArgumentIntegrity::Valid
}

pub(crate) trait WireTag: Copy + 'static {
    const ALL: &'static [Self];

    fn tag(self) -> &'static str;

    fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|value| value.tag() == tag)
    }
}

impl WireTag for ToolArgumentIntegrity {
    const ALL: &'static [Self] = &[Self::Valid, Self::MalformedJson, Self::NonObjectJson];

    fn tag(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::MalformedJson => "malformed_json",
            Self::NonObjectJson => "non_object_json",
        }
    }
}

impl WireTag for ToolExecutionProvenance {
    const ALL: &'static [Self] = &[Self::FxLocal, Self::ProviderExecuted];

    fn tag(self) -> &'static str {
        match self {
            Self::FxLocal => "fx_local",
            Self::ProviderExecuted => "provider_executed",
        }
    }
}

impl WireTag for ArtifactCompleteness {
    const ALL: &'static [Self] = &[Self::Complete, Self::Partial, Self::Unknown];

    fn tag(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
        }
    }
}

impl WireTag for InterruptReason {
    const ALL: &'static [Self] = &[Self::Cancelled, Self::Failed];

    fn tag(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

impl WireTag for FileEvidenceAction {
    const ALL: &'static [Self] = &Self::ALL;

    fn tag(self) -> &'static str {
        self.label()
    }
}

impl WireTag for ToolResultStatus {
    const ALL: &'static [Self] = &[Self::Success, Self::Failure];

    fn tag(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

mod wire_tag {
    use serde::Serializer;

    use super::WireTag;

    pub(super) fn serialize<S: Serializer, T: WireTag>(
        value: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.tag())
    }

    #[cfg(test)]
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>, T: WireTag>(
        deserializer: D,
    ) -> Result<T, D::Error> {
        let tag = <String as serde::Deserialize>::deserialize(deserializer)?;
        T::from_tag(&tag).ok_or_else(|| serde::de::Error::custom("InvalidEnumTag"))
    }
}

#[cfg(test)]
mod tests;
