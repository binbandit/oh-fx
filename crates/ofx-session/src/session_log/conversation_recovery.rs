use std::io::{self, Read, Write};

use ofx_config::PrivateDir;

use super::conversation_archive::ArchiveBuilder;
use super::managed_file::{Access, create_managed_file, open_managed_file};
use super::{ArchivedTurn, EVENTS_FILE};
use crate::session_error::SessionError;
use crate::session_event::{
    ConversationEvent, ConversationState, InterruptReason, InterruptedEvent,
    decode_conversation_frame, encode_conversation_frame,
};
use crate::session_replay::{History, LineRead, LineReader};
use crate::session_usage_sidecar::has_recoverable_corruption;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RecoveryBoundary {
    bytes: u64,
    seq: u64,
    timestamp_ms: i64,
    turn_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConversationRecovery {
    pub(crate) boundary: RecoveryBoundary,
    pub(crate) usage_incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveredArtifact {
    ToolOutput { handle: String, bytes: u64 },
    DiffContent { handle: String, call_id: String },
    CommandReplay { handle: String, bytes: u64 },
    CommandLog { handle: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveredLog {
    pub(crate) turns: Vec<ArchivedTurn>,
    pub(crate) artifacts: Vec<RecoveredArtifact>,
}

pub(crate) fn classify_conversation_recovery(
    dir: &PrivateDir,
    id: &str,
) -> Result<ConversationRecovery, SessionError> {
    let (boundary, complete) = scan_conversation_recovery(dir)?;
    let usage_incomplete = has_recoverable_corruption(dir, id)?;
    if complete && !usage_incomplete {
        return Err(SessionError::SessionRecoveryNotNeeded);
    }
    Ok(ConversationRecovery {
        boundary,
        usage_incomplete,
    })
}

fn scan_conversation_recovery(dir: &PrivateDir) -> Result<(RecoveryBoundary, bool), SessionError> {
    let file = open_managed_file(dir, EVENTS_FILE, Access::ReadOnly)?
        .ok_or(SessionError::SessionRecoveryBoundaryInvalid)?;
    let length = file.metadata()?.len();
    let mut lines = LineReader::new(&file, 0, length)?;
    let mut state = ConversationState::default();
    let mut builder = ArchiveBuilder::default();
    let mut boundary = RecoveryBoundary::default();
    let mut offset = 0;
    while let Ok(LineRead::Line(line)) = lines.next_line() {
        let Ok(envelope) = decode_conversation_frame(&line) else {
            break;
        };
        let (seq, timestamp_ms) = (envelope.seq, envelope.timestamp_ms());
        let checkpoint = matches!(envelope.event, ConversationEvent::ContextCheckpoint(_));
        if state.apply(seq, timestamp_ms, &envelope.event).is_err()
            || builder.apply(seq, envelope.event).is_err()
        {
            break;
        }
        offset = lines.offset();
        if !state.turn_open() || (checkpoint && !state.has_pending_tool_calls()) {
            boundary = RecoveryBoundary {
                bytes: offset,
                seq,
                timestamp_ms,
                turn_open: state.turn_open(),
            };
        }
    }
    let complete = offset == length && boundary.bytes == length;
    if !complete && boundary.bytes == 0 {
        return Err(SessionError::SessionRecoveryBoundaryInvalid);
    }
    Ok((boundary, complete))
}

pub(crate) fn recovered_log(
    dir: &PrivateDir,
    boundary: RecoveryBoundary,
) -> Result<RecoveredLog, SessionError> {
    let invalid = |_| SessionError::SessionRecoveryBoundaryInvalid;
    let file = open_managed_file(dir, EVENTS_FILE, Access::ReadOnly)?
        .ok_or(SessionError::SessionRecoveryBoundaryInvalid)?;
    let mut frames = History::log(&file).frames(0, boundary.bytes);
    let mut builder = ArchiveBuilder::default();
    let mut artifacts = Vec::new();
    while let Some(frame) = frames.next_frame().map_err(invalid)? {
        artifacts.extend(referenced_artifacts(&frame.envelope.event));
        builder
            .apply(frame.envelope.seq, frame.envelope.event)
            .map_err(invalid)?;
    }
    if frames.offset() != boundary.bytes {
        return Err(SessionError::SessionRecoveryBoundaryInvalid);
    }
    if boundary.turn_open {
        builder
            .apply(boundary.seq + 1, closing_interruption())
            .map_err(invalid)?;
    }
    Ok(RecoveredLog {
        turns: builder.into_turns(),
        artifacts,
    })
}

pub(crate) fn converted_log(events: &[ConversationEvent]) -> Result<RecoveredLog, SessionError> {
    let mut builder = ArchiveBuilder::default();
    let mut artifacts = Vec::new();
    for (seq, event) in (1_u64..).zip(events) {
        artifacts.extend(referenced_artifacts(event));
        builder
            .apply(seq, event.clone())
            .map_err(|_| SessionError::SessionRecoveryBoundaryInvalid)?;
    }
    Ok(RecoveredLog {
        turns: builder.into_turns(),
        artifacts,
    })
}

pub(crate) fn copy_conversation_recovery_prefix(
    source: &PrivateDir,
    target: &PrivateDir,
    boundary: RecoveryBoundary,
) -> Result<(), SessionError> {
    let input = open_managed_file(source, EVENTS_FILE, Access::ReadOnly)?
        .ok_or(SessionError::SessionRecoveryBoundaryInvalid)?;
    let mut output = create_managed_file(target, EVENTS_FILE)?;
    let copied = io::copy(&mut input.take(boundary.bytes), &mut output)?;
    if copied != boundary.bytes {
        return Err(SessionError::SessionRecoveryBoundaryInvalid);
    }
    if boundary.turn_open {
        output.write_all(&encode_conversation_frame(
            boundary.seq + 1,
            boundary.timestamp_ms,
            &closing_interruption(),
        )?)?;
    }
    output.sync_all()?;
    Ok(())
}

fn closing_interruption() -> ConversationEvent {
    ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None))
}

fn referenced_artifacts(event: &ConversationEvent) -> Vec<RecoveredArtifact> {
    let mut artifacts = Vec::new();
    let replay = |handle: &Option<String>, bytes: Option<u64>| {
        handle
            .clone()
            .map(|handle| RecoveredArtifact::CommandReplay {
                handle,
                bytes: bytes.unwrap_or_default(),
            })
    };
    match event {
        ConversationEvent::ToolResult(result) => {
            artifacts.push(RecoveredArtifact::ToolOutput {
                handle: result.artifact_ref.clone(),
                bytes: result.stored_bytes,
            });
            if let Some(handle) = result
                .committed_file_presentation
                .as_ref()
                .and_then(|presentation| presentation.content_handle.clone())
            {
                artifacts.push(RecoveredArtifact::DiffContent {
                    handle,
                    call_id: result.call_id.clone(),
                });
            }
            artifacts.extend(replay(
                &result.command_replay_ref,
                result.command_replay_bytes,
            ));
        }
        ConversationEvent::Interrupted(interrupted) => {
            artifacts.extend(replay(
                &interrupted.command_replay_ref,
                interrupted.command_replay_bytes,
            ));
            if let Some(handle) = interrupted.command_artifact_ref.clone() {
                artifacts.push(RecoveredArtifact::CommandLog { handle });
            }
        }
        ConversationEvent::User(_)
        | ConversationEvent::Assistant(_)
        | ConversationEvent::ToolCall(_)
        | ConversationEvent::Steering(_)
        | ConversationEvent::TurnCompleted(_)
        | ConversationEvent::ContextCheckpoint(_) => {}
    }
    artifacts
}

#[cfg(test)]
mod tests;
