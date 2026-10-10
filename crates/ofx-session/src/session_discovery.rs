use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

use ofx_config::PrivateDir;
use rustix::fs::{self, AtFlags, FileType};
use serde_json::Value;

use crate::session_children::has_owner_marker;
use crate::session_codec::{MAX_SESSION_METADATA_BYTES, SESSION_METADATA_SCHEMA_VERSION};
use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, ConversationState, decode_conversation_frame};
use crate::session_log::managed_file::{
    Access, entry_exists, file_type, has_private_dir_mode, open_managed_file, permissions,
    private_file_mode, read_managed_file, session_directory_names,
};
use crate::session_log::{
    EVENTS_FILE, MANIFEST_FILE, check_conversation, read_checkpoint, read_metadata,
};
use crate::session_migration::{holds_schema_v3, summarize_schema_v3};
use crate::session_replay::{LineRead, LineReader};
use crate::session_store_types::{DoctorDiagnostic, DoctorInspection, DoctorIssueKind};
use crate::session_summary_codec::{SessionSource, SessionSummary};

const NANOS_PER_MILLI: i64 = 1_000_000;
const RETIRED_LATEST_DIR: &str = "latest";
const MAX_MANAGED_NAME_BYTES: usize = 255;
const PENDING_TRANSITIONS: [&str; 2] = ["authority.pending.json", "commit.pending.json"];
const MANAGED_CHILDREN: [&[&str]; 4] = [
    &["logs", "commands"],
    &["artifacts", "browser"],
    &["tool-results"],
    &["subagent"],
];
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

pub(crate) fn inspect_for_doctor(
    sessions: &PrivateDir,
    limit: usize,
) -> Result<DoctorInspection, SessionError> {
    let mut inspection = DoctorInspection::default();
    for id in session_directory_names(sessions)? {
        if id == RETIRED_LATEST_DIR {
            continue;
        }
        if inspection.inspected_count >= limit {
            inspection.truncated = true;
            break;
        }
        inspection.inspected_count += 1;
        if let Some(kind) = inspect_doctor_session(sessions, &id) {
            inspection.diagnostics.push(DoctorDiagnostic {
                session_id: id,
                kind,
            });
        }
    }
    Ok(inspection)
}

fn inspect_doctor_session(sessions: &PrivateDir, id: &str) -> Option<DoctorIssueKind> {
    let inspected = match sessions.open_child(id) {
        Ok(Some(dir)) => inspect_session_dir(&dir, id),
        Ok(None) => Err(SessionError::SessionNotFound),
        Err(error) => Err(error.into()),
    };
    match inspected {
        Ok(issue) => issue,
        Err(SessionError::SessionPathUnsafe) => Some(DoctorIssueKind::UnsafePath),
        Err(_) => Some(DoctorIssueKind::CanonicalStateInvalid),
    }
}

fn inspect_session_dir(
    dir: &PrivateDir,
    id: &str,
) -> Result<Option<DoctorIssueKind>, SessionError> {
    if holds_conversation_metadata(dir)? {
        check_conversation(dir, id)?;
    } else if PENDING_TRANSITIONS
        .iter()
        .any(|name| entry_exists(dir, name).unwrap_or(false))
    {
        return Ok(Some(DoctorIssueKind::AuthorityTransitionPending));
    } else if holds_schema_v3(dir, id)? {
        summarize_schema_v3(dir, id)?;
    } else {
        return Err(SessionError::InvalidSessionFormat);
    }
    Ok(MANAGED_CHILDREN
        .iter()
        .any(|route| !managed_child_is_safe(dir, route))
        .then_some(DoctorIssueKind::UnsafePath))
}

pub(crate) fn holds_conversation_metadata(dir: &PrivateDir) -> Result<bool, SessionError> {
    let bytes = match read_managed_file(dir, MANIFEST_FILE, MAX_SESSION_METADATA_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) | Err(SessionError::InvalidSessionFormat) => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(serde_json::from_slice::<Value>(&bytes)
        .ok()
        .as_ref()
        .and_then(|metadata| metadata.get("schema_version"))
        .and_then(Value::as_u64)
        == Some(u64::from(SESSION_METADATA_SCHEMA_VERSION)))
}

fn managed_child_is_safe(parent: &PrivateDir, route: &[&str]) -> bool {
    let Some((component, rest)) = route.split_first() else {
        return managed_entries_are_safe(parent);
    };
    match parent.open_child(component) {
        Ok(None) => true,
        Ok(Some(child)) => {
            has_private_dir_mode(&child).unwrap_or(false) && managed_child_is_safe(&child, rest)
        }
        Err(_) => false,
    }
}

fn managed_entries_are_safe(dir: &PrivateDir) -> bool {
    let Ok(entries) = fs::Dir::read_from(dir.as_fd()) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let Some(name) = std::str::from_utf8(name)
            .ok()
            .filter(|name| is_managed_name(name))
        else {
            return false;
        };
        let private_file =
            fs::statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|stat| {
                file_type(&stat) == FileType::RegularFile
                    && stat.st_nlink == 1
                    && permissions(&stat) == private_file_mode()
            });
        if !private_file {
            return false;
        }
    }
    true
}

fn is_managed_name(name: &str) -> bool {
    (1..=MAX_MANAGED_NAME_BYTES).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests;
