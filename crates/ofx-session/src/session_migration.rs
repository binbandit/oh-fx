mod conversion;
mod durable_turn;
mod legacy_frame;

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
use crate::session_log::managed_file::{Access, open_managed_file, read_managed_file};
use crate::session_log::{EVENTS_FILE, read_metadata};
use crate::session_replay::{LineRead, LineReader};
use crate::session_summary_codec::SessionSummary;

pub(crate) use conversion::Converted;
use durable_turn::LegacyTurn;
use legacy_frame::{Envelope, Event, PreferenceChange, Started, decode_frame};

const WATERMARK_SCHEMA_VERSION: u64 = 1;

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
}

struct Watermark {
    seq: u64,
    event_id: Identifier,
    bytes: u64,
}

struct Replay {
    session: LegacySession,
    generation: Identifier,
    seq: u64,
    event_id: Identifier,
    recovery_set: bool,
}

pub(crate) fn holds_schema_v3(dir: &PrivateDir, id: &str) -> Result<bool, SessionError> {
    Ok(holds_authority_marker(dir)? && read_metadata(dir, id).is_err())
}

pub(crate) fn read_schema_v3(
    dir: &PrivateDir,
    id: &str,
) -> Result<Option<Converted>, SessionError> {
    let session = load_schema_v3(dir, id)?;
    if session.subagent_child || has_owner_marker(dir)? {
        return Ok(None);
    }
    session.convert().map(Some)
}

pub(crate) fn summarize_schema_v3(
    dir: &PrivateDir,
    id: &str,
) -> Result<Option<SessionSummary>, SessionError> {
    let Some(converted) = read_schema_v3(dir, id)? else {
        return Ok(None);
    };
    Ok(Some(converted.summary(read_sidecar_title(dir))))
}

fn load_schema_v3(dir: &PrivateDir, id: &str) -> Result<LegacySession, SessionError> {
    require_schema_v3(dir, id)?;
    let events = open_managed_file(dir, EVENTS_FILE, Access::ReadOnly)?
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
        && !replay.recovery_set;
    if committed {
        Ok(replay.session)
    } else {
        Err(SessionError::InvalidSessionFormat)
    }
}

fn read_watermark(
    dir: &PrivateDir,
    id: &str,
    generation: &Identifier,
) -> Result<Watermark, SessionError> {
    let name = format!("commit.{}.json", lowercase_hex(generation));
    let bytes = read_managed_file(dir, &name, MAX_CONTROL_FILE_BYTES)?
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

impl Replay {
    fn start(envelope: Envelope, id: &str) -> Result<Self, SessionError> {
        let Envelope {
            generation,
            seq: 1,
            event_id,
            timestamp_ms,
            event: Event::Started(started),
        } = envelope
        else {
            return Err(SessionError::InvalidSessionFormat);
        };
        if started.id != id {
            return Err(SessionError::InvalidSessionFormat);
        }
        Ok(Self {
            session: LegacySession::started(started, timestamp_ms),
            generation,
            seq: 1,
            event_id,
            recovery_set: false,
        })
    }

    fn apply(&mut self, envelope: Envelope) -> Result<(), SessionError> {
        if envelope.generation != self.generation || Some(envelope.seq) != self.seq.checked_add(1) {
            return Err(SessionError::InvalidSessionFormat);
        }
        let session = &mut self.session;
        match envelope.event {
            Event::Started(_) => return Err(SessionError::InvalidSessionFormat),
            Event::PreferencesChanged(change) => session.change_preferences(change),
            Event::WorkspaceRebound { previous, current } => {
                if previous != session.workspace_root || current == session.workspace_root {
                    return Err(SessionError::InvalidSessionFormat);
                }
                session.workspace_root = current;
            }
            Event::TurnCommitted {
                conversation_language,
                turn,
            } => {
                session.turns.push(turn);
                session.conversation_language = conversation_language;
                self.recovery_set = false;
            }
            Event::UsageCheckpointed => {}
            Event::RecoverySet => self.recovery_set = true,
            Event::RecoveryCleared => self.recovery_set = false,
        }
        session.updated_at_ms = envelope.timestamp_ms;
        self.seq = envelope.seq;
        self.event_id = envelope.event_id;
        Ok(())
    }
}

impl LegacySession {
    fn started(started: Started, timestamp_ms: i64) -> Self {
        Self {
            id: started.id,
            origin_workspace_root: started.origin_workspace_root,
            workspace_root: started.workspace_root,
            created_at_ms: started.created_at_ms,
            updated_at_ms: timestamp_ms,
            conversation_language: started.conversation_language,
            preferences: started.preferences,
            subagent_child: started.subagent_child,
            turns: Vec::new(),
        }
    }

    fn change_preferences(&mut self, change: PreferenceChange) {
        let preferences = &mut self.preferences;
        if let Some(provider) = change.provider {
            preferences.provider = provider;
        }
        if let Some(model) = change.model {
            preferences.model = model;
        }
        if let Some(effort) = change.effort {
            preferences.effort = effort;
        }
        if let Some(fast_mode) = change.fast_mode {
            preferences.fast_mode = fast_mode;
        }
        if let Some(ultrafast_mode) = change.ultrafast_mode {
            preferences.ultrafast_mode = ultrafast_mode;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
