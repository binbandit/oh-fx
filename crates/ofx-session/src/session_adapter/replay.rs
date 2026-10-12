use ofx_contract::ToolResultStatus;
use ofx_trace::trace_log;

use crate::session_error::SessionError;
use crate::session_event::{
    ConversationEvent, InterruptReason, InterruptedEvent, ToolCallEvent, decode_event_payload,
};
use crate::session_log::{ArchiveBuilder, ArchivedResult, ArchivedTurn, ExecutedStep};
use crate::session_manager::{BadBody, BlobError, Body, Fields, Manager, OpenError, Reason};
use crate::session_trace::SESSION;

const SUPERSEDED_TYPE: &str = "superseded";
const RUNNING_TYPE: &str = "tool_running";
const RUNNING_ASSISTANT_TYPE: &str = "assistant_running";
const UNFINISHED_TOOL_OUTPUT: &str = "oh-fx stopped while this tool was running, so it may have partly run. Check its effects before running it again.";
const BLOB_REF_PREFIX: &[u8] = b"{\"$blob\":\"";
const BLOB_REF_SUFFIX: &[u8] = b"\"}";
const BLOB_HASH_BYTES: usize = 64;

pub(super) enum ReplayError {
    Open(OpenError),
    Session(SessionError),
}

fn event_tag(item_type: &str) -> Option<&'static str> {
    Some(match item_type {
        "user" => "user",
        "assistant" | RUNNING_ASSISTANT_TYPE => "assistant",
        "tool_call" | RUNNING_TYPE => "tool_call",
        "tool_result" => "tool_result",
        "steering" => "steering",
        "turn_end" => "turn_completed",
        "interruption" => "interrupted",
        _ => return None,
    })
}

pub(super) fn detail_turns(manager: &Manager, id: &str) -> Result<Vec<ArchivedTurn>, ReplayError> {
    let mut replay = Replay {
        manager,
        id,
        builder: ArchiveBuilder::default(),
        interrupted_item: false,
        superseded: false,
        running: Vec::new(),
        represented: Vec::new(),
        running_text: None,
    };
    let damaged = manager
        .read_forward(id, |seq, body| replay.line(seq, body))
        .map_err(ReplayError::Open)?
        .map_err(ReplayError::Session)?;
    if damaged {
        trace_log!(
            SESSION,
            "event=sessions_v2_replay_damaged session={id} dropped=lines_after_damage"
        );
    }
    if !replay.builder.is_idle() {
        trace_log!(
            SESSION,
            "event=sessions_v2_replay_open_turn session={id} dropped=unfinished_pieces"
        );
    }
    Ok(replay.builder.into_turns())
}

struct Replay<'a> {
    manager: &'a Manager,
    id: &'a str,
    builder: ArchiveBuilder,
    interrupted_item: bool,
    superseded: bool,
    running: Vec<ToolCallEvent>,
    represented: Vec<String>,
    running_text: Option<String>,
}

impl Replay<'_> {
    fn line(&mut self, seq: u64, body: Option<Body<'_>>) -> Result<(), SessionError> {
        match body {
            Some(Body::TurnStarted { .. }) => {
                self.interrupted_item = false;
                self.superseded = false;
                self.running.clear();
                self.represented.clear();
                self.running_text = None;
            }
            Some(Body::Item {
                item_type,
                data,
                blobs,
            }) => self.item(seq, &item_type, data, &blobs)?,
            Some(Body::TurnInterrupted { turn, reason }) => self.interrupted(seq, turn, reason)?,
            Some(Body::Compacted { data }) => self.builder.push_summary(compacted_summary(data)?),
            _ => {}
        }
        Ok(())
    }

    fn item(
        &mut self,
        seq: u64,
        item_type: &str,
        data: &[u8],
        blobs: &[String],
    ) -> Result<(), SessionError> {
        if item_type == SUPERSEDED_TYPE {
            self.superseded = true;
            return Ok(());
        }
        let Some(tag) = event_tag(item_type) else {
            trace_log!(
                SESSION,
                "event=sessions_v2_unknown_item session={} type={item_type} dropped=item",
                self.id
            );
            return Ok(());
        };
        let payload = self.piece_data(data, blobs)?;
        let event = decode_event_payload(tag, &payload)?;
        match (item_type, event) {
            (RUNNING_TYPE, ConversationEvent::ToolCall(call)) => self.running.push(call),
            (RUNNING_ASSISTANT_TYPE, ConversationEvent::Assistant(assistant)) => {
                self.running_text = Some(assistant.text);
            }
            (_, event) => {
                match &event {
                    ConversationEvent::Assistant(_) => self.running_text = None,
                    ConversationEvent::ToolCall(call) => {
                        self.represented.push(call.call_id.clone());
                    }
                    ConversationEvent::ToolResult(result) => {
                        self.represented.push(result.call_id.clone());
                    }
                    ConversationEvent::Interrupted(_) => self.interrupted_item = true,
                    _ => {}
                }
                self.builder.apply(seq, event)?;
            }
        }
        Ok(())
    }

    fn interrupted(&mut self, seq: u64, turn: u64, reason: Reason) -> Result<(), SessionError> {
        if self.superseded {
            trace_log!(
                SESSION,
                "event=sessions_v2_replay_superseded session={} turn={turn} dropped=stale_turn",
                self.id
            );
            self.builder.discard_open_turn();
            self.superseded = false;
            return Ok(());
        }
        if self.interrupted_item || self.builder.is_idle() {
            return Ok(());
        }
        let reason = match reason {
            Reason::Cancel | Reason::Closed => InterruptReason::Cancelled,
            Reason::Failed | Reason::Crash => InterruptReason::Failed,
        };
        self.builder.apply(
            seq,
            ConversationEvent::Interrupted(InterruptedEvent::new(reason, None)),
        )?;
        let unanswered: Vec<ToolCallEvent> = self
            .running
            .iter()
            .filter(|call| !self.represented.contains(&call.call_id))
            .cloned()
            .collect();
        if unanswered.is_empty() {
            return Ok(());
        }
        trace_log!(
            SESSION,
            "event=sessions_v2_replay_unfinished_tools session={} turn={turn} calls={}",
            self.id,
            unanswered.len()
        );
        let results = unanswered.iter().map(unfinished_result).collect();
        self.builder.answer_last_interrupted(ExecutedStep {
            assistant: self.running_text.clone(),
            calls: unanswered,
            results,
        });
        Ok(())
    }

    fn piece_data(&self, data: &[u8], blobs: &[String]) -> Result<Vec<u8>, SessionError> {
        let Some(hash) = blob_ref(data) else {
            return Ok(data.to_vec());
        };
        if !blobs.iter().any(|listed| listed == hash) {
            return Ok(data.to_vec());
        }
        self.manager
            .get_blob(self.id, hash)
            .map_err(|error| match error {
                BlobError::NotFound | BlobError::Corrupt => {
                    trace_log!(
                        SESSION,
                        "event=sessions_v2_blob_unreadable session={} err={}",
                        self.id,
                        if error == BlobError::NotFound {
                            "NotFound"
                        } else {
                            "Corrupt"
                        }
                    );
                    SessionError::InvalidSessionFormat
                }
                BlobError::Fault(_) => SessionError::SessionStoreUnavailable,
            })
    }
}

fn blob_ref(data: &[u8]) -> Option<&str> {
    if data.len() != BLOB_REF_PREFIX.len() + BLOB_HASH_BYTES + BLOB_REF_SUFFIX.len() {
        return None;
    }
    let hash = data
        .strip_prefix(BLOB_REF_PREFIX)?
        .strip_suffix(BLOB_REF_SUFFIX)?;
    std::str::from_utf8(hash).ok()
}

fn unfinished_result(call: &ToolCallEvent) -> ArchivedResult {
    ArchivedResult {
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        status: ToolResultStatus::Failure,
        output: UNFINISHED_TOOL_OUTPUT.to_owned(),
        output_handle: None,
        preview: None,
        output_bytes: UNFINISHED_TOOL_OUTPUT.len() as u64,
        stored_output_bytes: UNFINISHED_TOOL_OUTPUT.len() as u64,
        truncated: false,
        provider_native: false,
        created_at_ms: 0,
        permission_feedback: Vec::new(),
        presentation: None,
    }
}

fn compacted_summary(data: &[u8]) -> Result<String, SessionError> {
    let members = Fields::struct_members(
        data,
        &[
            ("summary", true),
            ("removed_turn_count", true),
            ("compaction_count", true),
            ("keep_from_turn", false),
        ],
    )
    .map_err(|BadBody| SessionError::InvalidSessionFormat)?;
    let invalid = |_: BadBody| SessionError::InvalidSessionFormat;
    let summary = Fields::string_value(members[0].unwrap_or_default()).map_err(invalid)?;
    for count in &members[1..3] {
        Fields::u64_value(count.unwrap_or_default()).map_err(invalid)?;
    }
    if let Some(keep) = members[3].filter(|raw| *raw != b"null") {
        Fields::u64_value(keep).map_err(invalid)?;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests;
