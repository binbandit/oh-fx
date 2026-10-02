use std::os::unix::fs::MetadataExt;

use ofx_config::PrivateDir;

use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, decode_conversation_frame};
use crate::session_log::managed_file::{Access, open_managed_file};
use crate::session_log::{EVENTS_FILE, read_metadata};
use crate::session_replay::{LineRead, LineReader};
use crate::session_summary_codec::SessionSummary;

const NANOS_PER_MILLI: i64 = 1_000_000;
const MILLIS_PER_SECOND: i64 = 1_000;

pub(crate) fn classify_session(
    sessions: &PrivateDir,
    id: &str,
) -> Result<SessionSummary, SessionError> {
    let dir = sessions
        .open_child(id)?
        .ok_or(SessionError::SessionNotFound)?;
    let metadata = read_metadata(&dir, id)?;
    let file = open_managed_file(&dir, EVENTS_FILE, Access::ReadOnly)?
        .ok_or(SessionError::InvalidSessionFormat)?;
    let stat = file.metadata()?;
    let mut history_len: usize = 0;
    let mut has_checkpoint = false;
    let mut reader = LineReader::new(&file, 0, stat.len())?;
    while let LineRead::Line(line) = reader.next_line()? {
        match decode_conversation_frame(&line)?.event {
            ConversationEvent::ContextCheckpoint(_) => has_checkpoint = true,
            ConversationEvent::TurnCompleted(_) | ConversationEvent::Interrupted(_) => {
                history_len = history_len
                    .checked_add(1)
                    .ok_or(SessionError::InvalidSessionFormat)?;
            }
            _ => {}
        }
    }
    let updated_at_ms = if history_len == 0 && !has_checkpoint {
        metadata.updated_at_ms
    } else {
        metadata
            .updated_at_ms
            .max(modified_ms(stat.mtime(), stat.mtime_nsec()))
    };
    Ok(SessionSummary {
        id: metadata.id,
        workspace_root: metadata.workspace_root,
        origin_workspace_root: metadata.origin_workspace_root,
        title: metadata.title,
        created_at_ms: metadata.created_at_ms,
        updated_at_ms,
        conversation_language: metadata.conversation_language,
        history_len,
        has_checkpoint,
    })
}

fn modified_ms(seconds: i64, nanos: i64) -> i64 {
    seconds
        .saturating_mul(MILLIS_PER_SECOND)
        .saturating_add(nanos.div_euclid(NANOS_PER_MILLI))
}
