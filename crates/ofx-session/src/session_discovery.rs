use std::os::unix::fs::MetadataExt;

use ofx_config::PrivateDir;

use crate::session_children::has_owner_marker;
use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, ConversationState, decode_conversation_frame};
use crate::session_log::managed_file::{Access, open_managed_file};
use crate::session_log::{EVENTS_FILE, read_checkpoint, read_metadata};
use crate::session_migration::{holds_schema_v3, summarize_schema_v3};
use crate::session_replay::{LineRead, LineReader};
use crate::session_summary_codec::{SessionSource, SessionSummary};

const NANOS_PER_MILLI: i64 = 1_000_000;
const MILLIS_PER_SECOND: i64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Classification {
    Listing,
    Resume,
}

pub(crate) fn classify_session(
    sessions: &PrivateDir,
    id: &str,
    classification: Classification,
) -> Result<Option<SessionSummary>, SessionError> {
    let dir = sessions
        .open_child(id)?
        .ok_or(SessionError::SessionNotFound)?;
    if classification == Classification::Resume && holds_schema_v3(&dir, id)? {
        return summarize_schema_v3(&dir, id);
    }
    let metadata = read_metadata(&dir, id)?;
    if metadata.subagent_child || has_owner_marker(&dir)? {
        return Ok(None);
    }
    let file = open_managed_file(&dir, EVENTS_FILE, Access::ReadOnly)?
        .ok_or(SessionError::InvalidSessionFormat)?;
    let stat = file.metadata()?;
    let mut history_len: usize = 0;
    let mut has_checkpoint = false;
    let mut state = ConversationState::default();
    let mut open_turn_from: Option<u64> = None;
    let mut reader = LineReader::new(&file, 0, stat.len())?;
    while let LineRead::Line(line) = reader.next_line()? {
        let envelope = decode_conversation_frame(&line)?;
        let seq = envelope.seq;
        if classification == Classification::Resume {
            state.apply(seq, envelope.timestamp_ms(), &envelope.event)?;
        }
        match envelope.event {
            ConversationEvent::User(_) => open_turn_from = Some(seq.saturating_sub(1)),
            ConversationEvent::ContextCheckpoint(_) => {
                has_checkpoint = true;
                if open_turn_from.is_some() {
                    open_turn_from = Some(seq);
                }
            }
            ConversationEvent::TurnCompleted(_) | ConversationEvent::Interrupted(_) => {
                open_turn_from = None;
                history_len = history_len
                    .checked_add(1)
                    .ok_or(SessionError::InvalidSessionFormat)?;
            }
            _ => {}
        }
    }
    if classification == Classification::Resume {
        read_checkpoint(&dir, open_turn_from.unwrap_or(state.last_seq()))?;
    }
    let updated_at_ms = if history_len == 0 && !has_checkpoint {
        metadata.updated_at_ms
    } else {
        metadata
            .updated_at_ms
            .max(modified_ms(stat.mtime(), stat.mtime_nsec()))
    };
    Ok(Some(SessionSummary {
        id: metadata.id,
        workspace_root: metadata.workspace_root,
        origin_workspace_root: metadata.origin_workspace_root,
        title: metadata.title,
        created_at_ms: metadata.created_at_ms,
        updated_at_ms,
        conversation_language: metadata.conversation_language,
        history_len,
        has_checkpoint,
        source: SessionSource::OhFx,
    }))
}

fn modified_ms(seconds: i64, nanos: i64) -> i64 {
    seconds
        .saturating_mul(MILLIS_PER_SECOND)
        .saturating_add(nanos.div_euclid(NANOS_PER_MILLI))
}
