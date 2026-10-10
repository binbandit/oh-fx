use std::fs::File;
use std::io::{self, Read};
use std::os::fd::OwnedFd;

use ofx_config::{AdvisoryLock, PrivateDir};
use sha2::{Digest, Sha256};

use super::SessionStore;
use crate::artifact_digest::has_content_digest;
use crate::fx_sessions::{FxSessions, share_lock};
use crate::result_store::{DIFF_CONTENT_MAX_BYTES, diff_handle_matches_call};
use crate::session_children::has_owner_marker;
use crate::session_codec::{SessionMetadata, encode_session_metadata};
use crate::session_discovery::{holds_conversation_metadata, is_managed_name};
use crate::session_display_metadata::history_title;
use crate::session_error::SessionError;
use crate::session_layout::{generate_session_id, is_valid_session_id};
use crate::session_log::managed_file::{
    Access, create_managed_file, create_private_dir, lock_with_deadline, open_managed_file,
    publish_dir, remove_created_dir, sync_dir,
};
use crate::session_log::{
    ArchivedTurn, ConversationRecovery, MANIFEST_FILE, RecoveredArtifact, RecoveredLog,
    SESSION_LOCK_FILE, classify_conversation_recovery, converted_log,
    copy_conversation_recovery_prefix, load_archive, read_metadata, recovered_log, staging_name,
};
use crate::session_migration::{holds_schema_v3, read_schema_v3};
use crate::session_store_types::{SessionRecovery, SessionRecoveryStatus};
use crate::session_usage::UsageSnapshot;
use crate::session_usage_sidecar;

const TOOL_RESULTS: [&str; 1] = ["tool-results"];
const COMMAND_ARTIFACTS: [&str; 2] = ["logs", "commands"];
const REPLAY_PREFIX: &str = "fx-command-replay-";
const REPLAY_SUFFIX: &str = ".bin";
const COMMAND_LOG_PREFIX: &str = "fx-command-";
const COMMAND_LOG_SUFFIX: &str = ".log";

struct RecoverySource {
    dir: PrivateDir,
    _writer: Option<AdvisoryLock>,
    _shared: Option<OwnedFd>,
}

struct Recovered<'a> {
    metadata: &'a SessionMetadata,
    id: &'a str,
    recovery: ConversationRecovery,
    log: &'a RecoveredLog,
    usage: &'a UsageSnapshot,
}

impl SessionStore {
    pub fn recover(&self, id: &str) -> Result<SessionRecovery, SessionError> {
        if !is_valid_session_id(id) {
            return Err(SessionError::InvalidSessionId);
        }
        let sessions = self.writable_sessions()?;
        let RecoverySource { dir: source, .. } = &self.recovery_source(sessions, id)?;
        if !holds_conversation_metadata(source)? {
            if holds_schema_v3(source, id)? {
                return recover_schema_v3(sessions, source, id);
            }
            return Err(SessionError::SessionRecoveryRequiresCurrentSchema);
        }
        let metadata = read_metadata(source, id).map_err(|error| match error {
            SessionError::InvalidSessionMetadata | SessionError::InvalidSessionFormat => {
                SessionError::SessionRecoveryBoundaryInvalid
            }
            other => other,
        })?;
        if metadata.subagent_child {
            return Err(SessionError::SessionNotFound);
        }
        let recovery = match classify_conversation_recovery(source, id) {
            Err(SessionError::SessionRecoveryNotNeeded) => {
                load_archive(source, id, SessionError::SessionNotFound)?;
                return Err(SessionError::SessionRecoveryNotNeeded);
            }
            recovery => recovery?,
        };
        if has_owner_marker(source)? {
            return Err(SessionError::SessionNotFound);
        }
        let log = recovered_log(source, recovery.boundary)?;
        let usage = session_usage_sidecar::load_conversation(source, id, metadata.updated_at_ms)?;
        let recovered_id = generate_session_id().ok_or(SessionError::SessionStartFailed)?;
        let recovered = Recovered {
            metadata: &metadata,
            id: &recovered_id,
            recovery,
            log: &log,
            usage: &usage,
        };
        let unverified = publish_recovery(sessions, &recovered_id, |copy| {
            stage_recovery(source, copy, &recovered)
        })?;
        Ok(finish(
            sessions,
            SessionRecovery {
                source_session_id: id.to_owned(),
                recovered_session_id: recovered_id.clone(),
                history_len: log.turns.len(),
                usage_incomplete: recovery.usage_incomplete,
                status: SessionRecoveryStatus::Recovered,
            },
            &log,
            unverified,
        ))
    }

    fn recovery_source(
        &self,
        sessions: &PrivateDir,
        id: &str,
    ) -> Result<RecoverySource, SessionError> {
        match sessions.open_child(id) {
            Ok(Some(dir)) => {
                let lock = lock_with_deadline(&dir, SESSION_LOCK_FILE, self.lock_deadline)?
                    .ok_or(SessionError::SessionBusy)?;
                Ok(RecoverySource {
                    dir,
                    _writer: Some(lock),
                    _shared: None,
                })
            }
            Ok(None) => {
                let dir = self
                    .fx_home
                    .as_deref()
                    .map(FxSessions::open)
                    .and_then(|fx| fx.session_dir(id))
                    .ok_or(SessionError::SessionNotFound)?;
                let shared = share_lock(&dir)?;
                Ok(RecoverySource {
                    dir,
                    _writer: None,
                    _shared: shared,
                })
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn recover_schema_v3(
    sessions: &PrivateDir,
    source: &PrivateDir,
    id: &str,
) -> Result<SessionRecovery, SessionError> {
    let converted = match read_schema_v3(source, id) {
        Ok(Some(converted)) => converted,
        Ok(None) => return Err(SessionError::SessionNotFound),
        Err(_) => return Err(SessionError::SessionRecoveryBoundaryInvalid),
    };
    let recovered_id = generate_session_id().ok_or(SessionError::SessionStartFailed)?;
    let converted = converted.rebound(&recovered_id);
    let log = converted_log(converted.events())?;
    let unverified = publish_recovery(sessions, &recovered_id, |copy| {
        converted.write(copy)?;
        let mut unverified = false;
        for artifact in &log.artifacts {
            unverified |= copy_missing_artifact(source, copy, artifact)?;
        }
        Ok(unverified)
    })?;
    Ok(finish(
        sessions,
        SessionRecovery {
            source_session_id: id.to_owned(),
            recovered_session_id: recovered_id.clone(),
            history_len: converted.history_len(),
            usage_incomplete: false,
            status: SessionRecoveryStatus::Recovered,
        },
        &log,
        unverified,
    ))
}

fn finish(
    sessions: &PrivateDir,
    mut recovery: SessionRecovery,
    log: &RecoveredLog,
    unverified: bool,
) -> SessionRecovery {
    recovery.status = if sync_dir(sessions).is_err()
        || !verified(sessions, &recovery.recovered_session_id, log)
    {
        SessionRecoveryStatus::Indeterminate
    } else if unverified {
        SessionRecoveryStatus::RecoveredWithUnverifiedArtifacts
    } else {
        SessionRecoveryStatus::Recovered
    };
    recovery
}

fn publish_recovery(
    sessions: &PrivateDir,
    recovered_id: &str,
    stage: impl FnOnce(&PrivateDir) -> Result<bool, SessionError>,
) -> Result<bool, SessionError> {
    let staging = staging_name()?;
    create_private_dir(sessions, &staging).map_err(|_| SessionError::SessionStartFailed)?;
    let staged = sessions
        .open_child_private(&staging)
        .ok()
        .flatten()
        .ok_or(SessionError::SessionStartFailed)
        .and_then(|copy| stage(&copy));
    let unverified = match staged {
        Ok(unverified) => unverified,
        Err(error) => {
            remove_created_dir(sessions, &staging);
            return Err(error);
        }
    };
    if publish_dir(sessions, &staging, recovered_id).is_err() {
        remove_created_dir(sessions, &staging);
        return Err(SessionError::SessionRecoveryIndeterminate);
    }
    Ok(unverified)
}

fn stage_recovery(
    source: &PrivateDir,
    copy: &PrivateDir,
    recovered: &Recovered<'_>,
) -> Result<bool, SessionError> {
    let mut unverified = false;
    for artifact in &recovered.log.artifacts {
        unverified |= copy_artifact(source, copy, artifact)?;
    }
    copy_conversation_recovery_prefix(source, copy, recovered.recovery.boundary)?;
    let turns = &recovered.log.turns;
    let manifest = SessionMetadata {
        id: recovered.id.to_owned(),
        title: recovered
            .metadata
            .title
            .clone()
            .or_else(|| history_title(turns.len(), turns.iter().filter_map(prompt))),
        ..recovered.metadata.clone()
    };
    copy.replace(MANIFEST_FILE, &encode_session_metadata(&manifest)?)?;
    session_usage_sidecar::write(copy, recovered.id, recovered.usage)?;
    sync_dir(copy)?;
    Ok(unverified)
}

fn prompt(turn: &ArchivedTurn) -> Option<&str> {
    match turn {
        ArchivedTurn::Replied { user, .. } | ArchivedTurn::Interrupted { user, .. } => {
            Some(user.as_str())
        }
        ArchivedTurn::Compacted(_) => None,
    }
}

fn verified(sessions: &PrivateDir, id: &str, log: &RecoveredLog) -> bool {
    sessions
        .open_child(id)
        .ok()
        .flatten()
        .and_then(|dir| load_archive(&dir, id, SessionError::SessionNotFound).ok())
        .is_some_and(|archive| archive.turns == log.turns)
}

fn copy_missing_artifact(
    source: &PrivateDir,
    target: &PrivateDir,
    artifact: &RecoveredArtifact,
) -> Result<bool, SessionError> {
    let converted = match artifact {
        RecoveredArtifact::ToolOutput { handle, .. }
        | RecoveredArtifact::DiffContent { handle, .. } => {
            stored_in(target, &TOOL_RESULTS, handle)?
        }
        RecoveredArtifact::CommandReplay { .. } | RecoveredArtifact::CommandLog { .. } => false,
    };
    if converted {
        return Ok(false);
    }
    copy_artifact(source, target, artifact)
}

fn stored_in(session: &PrivateDir, route: &[&str], handle: &str) -> Result<bool, SessionError> {
    if !is_managed_name(handle) {
        return Err(SessionError::SessionRecoveryBoundaryInvalid);
    }
    Ok(match open_route(session, route)? {
        Some(dir) => open_managed_file(&dir, handle, Access::ReadOnly)?.is_some(),
        None => false,
    })
}

fn copy_artifact(
    source: &PrivateDir,
    target: &PrivateDir,
    artifact: &RecoveredArtifact,
) -> Result<bool, SessionError> {
    match artifact {
        RecoveredArtifact::ToolOutput { handle, bytes } => {
            copy_managed(source, target, &TOOL_RESULTS, handle, Some(*bytes), None)?;
            Ok(false)
        }
        RecoveredArtifact::DiffContent { handle, call_id } => {
            if !diff_handle_matches_call(handle, call_id) {
                return Err(SessionError::SessionRecoveryBoundaryInvalid);
            }
            let limit = u64::try_from(DIFF_CONTENT_MAX_BYTES).unwrap_or(u64::MAX);
            copy_managed(source, target, &TOOL_RESULTS, handle, None, Some(limit))?;
            Ok(false)
        }
        RecoveredArtifact::CommandReplay { handle, bytes } => {
            copy_managed(
                source,
                target,
                &COMMAND_ARTIFACTS,
                handle,
                Some(*bytes),
                None,
            )?;
            Ok(!authenticated(handle, REPLAY_PREFIX, REPLAY_SUFFIX))
        }
        RecoveredArtifact::CommandLog { handle } => {
            copy_managed(source, target, &COMMAND_ARTIFACTS, handle, None, None)?;
            Ok(!authenticated(
                handle,
                COMMAND_LOG_PREFIX,
                COMMAND_LOG_SUFFIX,
            ))
        }
    }
}

fn authenticated(handle: &str, prefix: &str, suffix: &str) -> bool {
    handle.starts_with(prefix) && handle.ends_with(suffix) && has_content_digest(handle, suffix)
}

fn copy_managed(
    source: &PrivateDir,
    target: &PrivateDir,
    route: &[&str],
    handle: &str,
    expected_bytes: Option<u64>,
    max_bytes: Option<u64>,
) -> Result<(), SessionError> {
    let invalid = SessionError::SessionRecoveryBoundaryInvalid;
    if !is_managed_name(handle) {
        return Err(invalid);
    }
    let source_dir = open_route(source, route)?.ok_or(invalid)?;
    let mut input = open_managed_file(&source_dir, handle, Access::ReadOnly)?.ok_or(invalid)?;
    let size = input.metadata()?.len();
    if expected_bytes.is_some_and(|expected| expected != size)
        || max_bytes.is_some_and(|limit| size > limit)
    {
        return Err(invalid);
    }
    let target_dir = create_route(target, route)?;
    match create_managed_file(&target_dir, handle) {
        Ok(mut output) => {
            io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        }
        Err(SessionError::SessionAlreadyExists) => {
            let existing =
                open_managed_file(&target_dir, handle, Access::ReadOnly)?.ok_or(invalid)?;
            if existing.metadata()?.len() != size || digest(existing)? != digest(input)? {
                return Err(invalid);
            }
        }
        Err(error) => return Err(error),
    }
    sync_dir(&target_dir)
}

fn digest(mut file: File) -> Result<[u8; 32], SessionError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..read]);
    }
}

fn open_route(session: &PrivateDir, route: &[&str]) -> Result<Option<PrivateDir>, SessionError> {
    let mut dir = session.try_clone()?;
    for component in route {
        match dir.open_child(component)? {
            Some(child) => dir = child,
            None => return Ok(None),
        }
    }
    Ok(Some(dir))
}

fn create_route(session: &PrivateDir, route: &[&str]) -> Result<PrivateDir, SessionError> {
    let mut dir = session.try_clone()?;
    for component in route {
        dir = dir.open_or_create_child(component)?;
    }
    Ok(dir)
}
