mod conversion;
mod durable_state;
mod durable_turn;
mod legacy_checkpoint;
mod legacy_frame;
mod legacy_presentation;
mod recovery_file;
mod replay;

use ofx_config::PrivateDir;
use ofx_text::lowercase_hex;

use crate::json_fields::{Fields, Json, parse_json};
use crate::session_authority::{
    Identifier, MAX_CONTROL_FILE_BYTES, holds_authority_marker, parse_identifier, require_schema_v3,
};
use crate::session_children::has_owner_marker;
use crate::session_codec::SessionPreferences;
use crate::session_display_metadata::read_sidecar_title;
use crate::session_error::SessionError;
use crate::session_log::managed_file::{
    Access, entry_exists, open_managed_file, read_managed_file,
};
use crate::session_log::{EVENTS_FILE, read_metadata};
use crate::session_replay::{LineRead, LineReader};
use crate::session_summary_codec::SessionSummary;
use crate::session_usage::UsageSnapshot;

pub(crate) use conversion::Converted;
use conversion::Purpose;
use durable_turn::LegacyTurn;
use legacy_checkpoint::{Continuable, LegacyCheckpoint};
use legacy_frame::decode_frame;
use replay::Replay;

const WATERMARK_SCHEMA_VERSION: u64 = 1;
const UPGRADE_BACKUP_FILE: &str = "events.v3.backup";

pub(crate) struct LegacySession {
    id: String,
    origin_workspace_root: String,
    workspace_root: String,
    created_at_ms: i64,
    updated_at_ms: i64,
    conversation_language: String,
    preferences: SessionPreferences,
    subagent_child: bool,
    turns: Vec<LegacyTurn>,
    context_history_start: usize,
    recovery: Option<Box<Continuable>>,
    usage: Option<UsageSnapshot>,
}

struct Watermark {
    seq: u64,
    event_id: Identifier,
    bytes: u64,
}

pub(crate) fn holds_schema_v3(dir: &PrivateDir, id: &str) -> Result<bool, SessionError> {
    if !holds_authority_marker(dir)? {
        return Ok(false);
    }
    Ok(!matches!(
        read_metadata(dir, id),
        Ok(_) | Err(SessionError::InvalidSessionMetadata)
    ))
}

pub(crate) fn read_schema_v3(
    dir: &PrivateDir,
    id: &str,
) -> Result<Option<Converted>, SessionError> {
    converted(dir, id, Purpose::Import)
}

pub(crate) fn summarize_schema_v3(
    dir: &PrivateDir,
    id: &str,
) -> Result<Option<SessionSummary>, SessionError> {
    let Some(converted) = converted(dir, id, Purpose::Listing)? else {
        return Ok(None);
    };
    Ok(Some(converted.summary(read_sidecar_title(dir))))
}

fn converted(
    dir: &PrivateDir,
    id: &str,
    purpose: Purpose,
) -> Result<Option<Converted>, SessionError> {
    let session = load_schema_v3(dir, id)?;
    if session.subagent_child || has_owner_marker(dir)? {
        return Ok(None);
    }
    session.convert(purpose).map(Some)
}

pub(crate) fn source_log(dir: &PrivateDir, id: &str) -> Result<&'static str, SessionError> {
    if holds_schema_v3(dir, id)? {
        schema_v3_log(dir)
    } else {
        Ok(EVENTS_FILE)
    }
}

fn schema_v3_log(dir: &PrivateDir) -> Result<&'static str, SessionError> {
    Ok(if entry_exists(dir, UPGRADE_BACKUP_FILE)? {
        UPGRADE_BACKUP_FILE
    } else {
        EVENTS_FILE
    })
}

pub(crate) fn schema_v3_watermark(
    dir: &PrivateDir,
    id: &str,
) -> Result<Option<String>, SessionError> {
    if !holds_schema_v3(dir, id)? {
        return Ok(None);
    }
    let Some(events) = open_managed_file(dir, schema_v3_log(dir)?, Access::ReadOnly)? else {
        return Ok(None);
    };
    let length = events.metadata()?.len();
    let LineRead::Line(first) = LineReader::new(&events, 0, length)?.next_line()? else {
        return Ok(None);
    };
    Ok(decode_frame(&first)
        .ok()
        .map(|envelope| watermark_name(&envelope.generation)))
}

fn watermark_name(generation: &Identifier) -> String {
    format!("commit.{}.json", lowercase_hex(generation))
}

fn load_schema_v3(dir: &PrivateDir, id: &str) -> Result<LegacySession, SessionError> {
    require_schema_v3(dir, id)?;
    let events = open_managed_file(dir, schema_v3_log(dir)?, Access::ReadOnly)?
        .ok_or(SessionError::InvalidSessionFormat)?;
    let length = events.metadata()?.len();
    let mut reader = LineReader::new(&events, 0, length)?;
    let LineRead::Line(first) = reader.next_line()? else {
        return Err(SessionError::InvalidSessionFormat);
    };
    let mut replay = Replay::start(decode_frame(&first)?, id)?;
    let watermark = read_watermark(dir, id, &replay.generation)?;
    while reader.offset() < watermark.bytes {
        let LineRead::Line(line) = reader.next_line()? else {
            return Err(SessionError::InvalidSessionFormat);
        };
        replay.apply(decode_frame(&line)?)?;
    }
    let committed = replay.seq == watermark.seq
        && replay.event_id == watermark.event_id
        && reader.offset() == watermark.bytes
        && replay.settled();
    if !committed {
        return Err(SessionError::InvalidSessionFormat);
    }
    let mut session = replay.session;
    match replay.recovery {
        Some(LegacyCheckpoint::Archived(turn)) => {
            session.turns.push(LegacyTurn::Conversation(turn));
        }
        Some(LegacyCheckpoint::Continuable(checkpoint)) => session.recovery = Some(checkpoint),
        None => {}
    }
    Ok(session)
}

fn read_watermark(
    dir: &PrivateDir,
    id: &str,
    generation: &Identifier,
) -> Result<Watermark, SessionError> {
    let bytes = read_managed_file(dir, &watermark_name(generation), MAX_CONTROL_FILE_BYTES)?
        .ok_or(SessionError::InvalidSessionFormat)?;
    let document = parse_json(&bytes).map_err(|_| SessionError::InvalidSessionFormat)?;
    watermark(document, id, generation).ok_or(SessionError::InvalidSessionFormat)
}

fn watermark(document: Json<'_>, id: &str, generation: &Identifier) -> Option<Watermark> {
    let mut fields = Fields::new(document)?;
    (fields.unsigned("schema_version")? == WATERMARK_SCHEMA_VERSION).then_some(())?;
    (fields.string("session_id")? == id).then_some(())?;
    (parse_identifier(&fields.string("log_generation")?)? == *generation).then_some(())?;
    let watermark = Watermark {
        seq: fields.unsigned("through_seq")?,
        event_id: parse_identifier(&fields.string("through_event_id")?)?,
        bytes: fields
            .unsigned("through_event_log_bytes")
            .filter(|bytes| *bytes > 0)?,
    };
    fields.finish(watermark)
}

#[cfg(test)]
pub(crate) mod tests;
