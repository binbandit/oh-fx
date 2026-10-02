mod conversation_history;
mod conversation_writer;
pub(crate) mod managed_file;

use std::fmt::Write as _;
use std::mem;
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir};

use crate::session_codec::{
    MAX_SESSION_METADATA_BYTES, SessionMetadata, SessionPreferences, decode_session_metadata,
    encode_session_metadata,
};
use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, InterruptReason, InterruptedEvent};
use crate::session_layout::is_valid_session_id;

pub use conversation_history::{CompactedHistory, SavedHistory, SavedTurn};
use conversation_history::{ReplayScan, replay_history};
use conversation_writer::{ConversationWriter, scan_log};
use managed_file::{
    Access, create_managed_file, create_private_dir, entry_exists, lock_with_deadline,
    open_managed_file, publish_dir, read_managed_file, remove_created_dir, sync_dir,
};

pub(crate) const EVENTS_FILE: &str = "events.jsonl";
const MANIFEST_FILE: &str = "session.json";
const SESSION_LOCK_FILE: &str = "session.lock";
const OWNER_LIVE_FILE: &str = "owner.live";
const STAGING_PREFIX: &str = "creating+";
const STAGING_RANDOM_BYTES: usize = 16;
pub(crate) const LOCK_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSession {
    pub metadata: SessionMetadata,
    pub history: SavedHistory,
}

struct OwnedSessionDir {
    dir: PrivateDir,
    previous_owner_died: bool,
    _lock: AdvisoryLock,
}

impl OwnedSessionDir {
    fn acquire(dir: PrivateDir, lock_deadline: Duration) -> Result<Self, SessionError> {
        let lock = lock_with_deadline(&dir, SESSION_LOCK_FILE, lock_deadline)?;
        let previous_owner_died = entry_exists(&dir, OWNER_LIVE_FILE).unwrap_or(false);
        let marker = format!(
            "{{\"pid\":{},\"opened_at_ms\":{}}}\n",
            process::id(),
            now_ms()
        );
        let _ = dir.replace(OWNER_LIVE_FILE, marker.as_bytes());
        Ok(Self {
            dir,
            previous_owner_died,
            _lock: lock,
        })
    }
}

impl Drop for OwnedSessionDir {
    fn drop(&mut self) {
        let _ = self.dir.remove(OWNER_LIVE_FILE);
    }
}

pub struct WritableSession {
    owned: OwnedSessionDir,
    writer: ConversationWriter,
    metadata: SessionMetadata,
    history: SavedHistory,
}

impl WritableSession {
    pub fn id(&self) -> &str {
        &self.metadata.id
    }

    pub fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }

    pub fn previous_owner_died(&self) -> bool {
        self.owned.previous_owner_died
    }

    pub fn turn_open(&self) -> bool {
        self.writer.turn_open()
    }

    pub fn take_history(&mut self) -> SavedHistory {
        mem::take(&mut self.history)
    }

    pub fn append(
        &mut self,
        timestamp_ms: i64,
        events: &[ConversationEvent],
    ) -> Result<(), SessionError> {
        self.writer.append(timestamp_ms, events)?;
        if !events.is_empty() {
            self.metadata.updated_at_ms = timestamp_ms;
        }
        Ok(())
    }

    pub fn set_preferences(
        &mut self,
        preferences: SessionPreferences,
        timestamp_ms: i64,
    ) -> Result<(), SessionError> {
        let mut proposed = self.metadata.clone();
        proposed.preferences = preferences;
        proposed.updated_at_ms = timestamp_ms;
        self.write_metadata(proposed)
    }

    pub(crate) fn rebind_workspace(&mut self, workspace_root: &str) -> Result<(), SessionError> {
        let mut proposed = self.metadata.clone();
        workspace_root.clone_into(&mut proposed.workspace_root);
        proposed.updated_at_ms = now_ms();
        self.write_metadata(proposed)
    }

    fn write_metadata(&mut self, proposed: SessionMetadata) -> Result<(), SessionError> {
        if let Some(failure) = self.writer.failure() {
            return Err(failure);
        }
        let bytes = encode_session_metadata(&proposed)?;
        match self.owned.dir.replace(MANIFEST_FILE, &bytes) {
            Ok(()) => {
                self.metadata = proposed;
                Ok(())
            }
            Err(DurableError::PostRenameFailed) => {
                self.writer.mark_uncertain();
                Err(SessionError::SessionPersistenceUncertain)
            }
            Err(error) => Err(error.into()),
        }
    }
}

pub(crate) fn start_session(
    sessions: &PrivateDir,
    metadata: SessionMetadata,
) -> Result<WritableSession, SessionError> {
    let manifest = encode_session_metadata(&metadata)?;
    if entry_exists(sessions, &metadata.id)? {
        return Err(SessionError::SessionAlreadyExists);
    }
    let staging = staging_name()?;
    create_private_dir(sessions, &staging).map_err(|_| SessionError::SessionStartFailed)?;
    let prepared =
        prepare_session(sessions, &staging, &manifest).map(|(owned, writer)| WritableSession {
            owned,
            writer,
            metadata,
            history: SavedHistory::default(),
        });
    let session = match prepared {
        Ok(session) => session,
        Err(error) => {
            remove_created_dir(sessions, &staging);
            return Err(error);
        }
    };
    let id = session.metadata.id.clone();
    if let Err(error) = publish_dir(sessions, &staging, &id) {
        drop(session);
        remove_created_dir(sessions, &staging);
        return Err(error);
    }
    sync_dir(sessions).map_err(|_| SessionError::SessionStartFailed)?;
    Ok(session)
}

fn prepare_session(
    sessions: &PrivateDir,
    staging: &str,
    manifest: &[u8],
) -> Result<(OwnedSessionDir, ConversationWriter), SessionError> {
    let dir = sessions
        .open_child_private(staging)?
        .ok_or(SessionError::SessionStartFailed)?;
    let owned = OwnedSessionDir::acquire(dir, LOCK_DEADLINE)?;
    owned.dir.replace(MANIFEST_FILE, manifest)?;
    let file = create_managed_file(&owned.dir, EVENTS_FILE)?;
    file.sync_all()?;
    sync_dir(&owned.dir)?;
    Ok((owned, ConversationWriter::new(file)))
}

pub(crate) fn resume_session(
    sessions: &PrivateDir,
    id: &str,
    lock_deadline: Duration,
) -> Result<WritableSession, SessionError> {
    if !is_valid_session_id(id) {
        return Err(SessionError::InvalidSessionId);
    }
    let dir = sessions
        .open_child_private(id)?
        .ok_or(SessionError::SessionNotFound)?;
    let owned = OwnedSessionDir::acquire(dir, lock_deadline)?;
    let metadata = read_metadata(&owned.dir, id)?;
    let file = open_managed_file(&owned.dir, EVENTS_FILE, Access::Writable)?
        .ok_or(SessionError::InvalidSessionFormat)?;
    let mut replay = ReplayScan::default();
    let mut writer = ConversationWriter::open(file, &mut replay)?;
    if writer.turn_open() {
        let offset = writer.committed_bytes();
        let interrupted =
            ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None));
        writer.append(now_ms(), std::slice::from_ref(&interrupted))?;
        replay.observe(offset, writer.last_seq(), &interrupted)?;
    }
    let end = writer.committed_bytes();
    let window = replay.finish(writer.file(), end)?;
    let history = replay_history(writer.file(), end, &window)?;
    Ok(WritableSession {
        owned,
        writer,
        metadata,
        history,
    })
}

pub(crate) fn load_session(sessions: &PrivateDir, id: &str) -> Result<SavedSession, SessionError> {
    if !is_valid_session_id(id) {
        return Err(SessionError::InvalidSessionId);
    }
    let dir = sessions
        .open_child(id)?
        .ok_or(SessionError::SessionNotFound)?;
    let metadata = read_metadata(&dir, id)?;
    let file = open_managed_file(&dir, EVENTS_FILE, Access::ReadOnly)?
        .ok_or(SessionError::InvalidSessionFormat)?;
    let length = file.metadata()?.len();
    let mut replay = ReplayScan::default();
    let scan = scan_log(&file, length, &mut replay)?;
    let window = replay.finish(&file, scan.complete_bytes)?;
    let history = replay_history(&file, scan.complete_bytes, &window)?;
    Ok(SavedSession { metadata, history })
}

pub(crate) fn read_metadata(dir: &PrivateDir, id: &str) -> Result<SessionMetadata, SessionError> {
    let bytes = read_managed_file(dir, MANIFEST_FILE, MAX_SESSION_METADATA_BYTES)?
        .ok_or(SessionError::SessionNotFound)?;
    let metadata = decode_session_metadata(&bytes).map_err(|error| match error {
        SessionError::SessionMetadataTooLarge => SessionError::InvalidSessionFormat,
        other => other,
    })?;
    if metadata.id != id {
        return Err(SessionError::InvalidSessionMetadata);
    }
    Ok(metadata)
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn staging_name() -> Result<String, SessionError> {
    let mut random = [0_u8; STAGING_RANDOM_BYTES];
    getrandom::fill(&mut random).map_err(|_| SessionError::SessionStartFailed)?;
    let mut name = String::from(STAGING_PREFIX);
    for byte in random {
        let _ = write!(name, "{byte:02x}");
    }
    Ok(name)
}

#[cfg(test)]
mod tests;
