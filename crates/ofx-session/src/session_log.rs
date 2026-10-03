mod conversation_history;
mod conversation_progress;
mod conversation_writer;
pub(crate) mod managed_file;
mod turn_events;
mod turn_restore;

use std::mem;
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir};
use ofx_contract::{HistoryCut, HistoryTurn, ReasoningEffort, RestoredHistory, TurnEnd, TurnStop};
use ofx_text::lowercase_hex;

use crate::session_codec::{
    MAX_SESSION_METADATA_BYTES, SavedProvider, SessionMetadata, SessionPreferences,
    decode_session_metadata, encode_session_metadata,
};
use crate::session_display_metadata::derive_display_title;
use crate::session_error::SessionError;
use crate::session_event::{
    ContextCheckpointEvent, ConversationEvent, InterruptReason, InterruptedEvent, ToolResultEvent,
};
use crate::session_layout::is_valid_session_id;

pub use conversation_history::{CompactedHistory, SavedHistory, SavedTurn};
use conversation_history::{ReplayScan, replay_history, visit_turns};
use conversation_progress::ProgressPoint;
use conversation_writer::{ConversationWriter, scan_log};
use managed_file::{
    Access, create_managed_file, create_private_dir, entry_exists, lock_with_deadline,
    open_managed_file, publish_dir, read_managed_file, remove_created_dir, remove_session_dir,
    same_directory, sync_dir,
};
use turn_events::{TurnArtifacts, turn_events};
use turn_restore::{complete_result_output, restored_history};

pub(crate) const EVENTS_FILE: &str = "events.jsonl";
const MANIFEST_FILE: &str = "session.json";
const SESSION_LOCK_FILE: &str = "session.lock";
const OWNER_LIVE_FILE: &str = "owner.live";
const STAGING_PREFIX: &str = "creating+";
const STAGING_RANDOM_BYTES: usize = 16;
pub(crate) const LOCK_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDisposal {
    Discarded,
    Retained,
    Indeterminate,
}

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
        let lock = lock_with_deadline(&dir, SESSION_LOCK_FILE, lock_deadline)?
            .ok_or(SessionError::SessionBusy)?;
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
    started: bool,
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

    pub fn last_seq(&self) -> u64 {
        self.writer.last_seq()
    }

    pub fn require_writable(&self) -> Result<(), SessionError> {
        self.writer.failure().map_or(Ok(()), Err)
    }

    pub fn visit_transcript(&self, visit: impl FnMut(SavedTurn)) -> Result<(), SessionError> {
        visit_turns(self.writer.file(), self.writer.committed_bytes(), visit)
    }

    pub fn display_title(&self) -> String {
        match &self.metadata.title {
            Some(title) => title.clone(),
            None => derive_display_title(&self.history),
        }
    }

    pub fn take_history(&mut self) -> SavedHistory {
        mem::take(&mut self.history)
    }

    pub fn tool_result_output(&self, result: &ToolResultEvent) -> Option<String> {
        complete_result_output(result, &self.owned.dir)
    }

    pub fn restored_history(&mut self) -> Result<RestoredHistory, SessionError> {
        restored_history(mem::take(&mut self.history), &self.owned.dir)
    }

    pub fn record_turn(
        &mut self,
        turn: &HistoryTurn<'_>,
        provider: &SavedProvider,
    ) -> Result<(), SessionError> {
        let open = self.writer.turn_open();
        let nothing_done = turn.steps.is_empty()
            && turn.end
                == TurnEnd::Stopped {
                    reason: TurnStop::Failed,
                    partial: "",
                };
        if nothing_done && !open {
            return Ok(());
        }
        let saved = self.append_turn(turn, provider, open);
        if saved.is_err() && self.writer.turn_open() {
            self.writer.block_open_turn();
        }
        saved
    }

    fn append_turn(
        &mut self,
        turn: &HistoryTurn<'_>,
        provider: &SavedProvider,
        open: bool,
    ) -> Result<(), SessionError> {
        let timestamp_ms = now_ms();
        let written = self.written_steps()?;
        let events = turn_events(&self.artifacts(provider, timestamp_ms), turn, written)?;
        self.append(timestamp_ms, &events[usize::from(open)..])
    }

    pub fn record_compaction(
        &mut self,
        summary: &str,
        cut: HistoryCut,
        active: Option<&HistoryTurn<'_>>,
        provider: &SavedProvider,
    ) -> Result<(), SessionError> {
        self.require_writable()?;
        let timestamp_ms = now_ms();
        let mut events = match active {
            Some(active) => self.active_prefix(active, provider, timestamp_ms)?,
            None => Vec::new(),
        };
        let covers_through_seq = self
            .writer
            .context_coverage(ProgressPoint::from(cut), &events)?;
        events.push(ConversationEvent::ContextCheckpoint(
            ContextCheckpointEvent {
                covers_through_seq,
                summary: summary.to_owned(),
            },
        ));
        self.append(timestamp_ms, &events)
    }

    fn active_prefix(
        &self,
        active: &HistoryTurn<'_>,
        provider: &SavedProvider,
        timestamp_ms: i64,
    ) -> Result<Vec<ConversationEvent>, SessionError> {
        let replied_nothing = TurnEnd::Replied {
            text: "",
            provider_replay: None,
        };
        if active.end != replied_nothing {
            return Err(SessionError::InvalidConversationEvent);
        }
        let written = self.written_steps()?;
        let mut events = turn_events(&self.artifacts(provider, timestamp_ms), active, written)?;
        events.pop();
        events.drain(..usize::from(self.writer.turn_open()));
        Ok(events)
    }

    pub(crate) fn is_pristine(&self) -> bool {
        self.started && self.writer.last_seq() == 0 && !self.writer.turn_open()
    }

    fn written_steps(&self) -> Result<usize, SessionError> {
        if !self.writer.turn_open() {
            return Ok(0);
        }
        Ok(self.writer.context_progress(None)?.point.tool_steps)
    }

    fn artifacts<'a>(
        &'a self,
        provider: &'a SavedProvider,
        timestamp_ms: i64,
    ) -> TurnArtifacts<'a> {
        TurnArtifacts {
            dir: &self.owned.dir,
            provider,
            timestamp_ms,
        }
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
        self.write_metadata(proposed)?;
        self.started = false;
        Ok(())
    }

    pub fn select_model(
        &mut self,
        model: &str,
        effort: Option<&ReasoningEffort>,
        fast_mode: bool,
    ) -> Result<(), SessionError> {
        let mut preferences = self.metadata.preferences.clone();
        model.clone_into(&mut preferences.model);
        if let Some(effort) = effort {
            effort.clone_into(&mut preferences.effort);
        }
        preferences.fast_mode = fast_mode;
        self.set_preferences(preferences, now_ms())
    }

    pub fn select_provider(
        &mut self,
        provider: SavedProvider,
        model: &str,
    ) -> Result<(), SessionError> {
        let mut preferences = self.metadata.preferences.clone();
        preferences.provider = provider;
        model.clone_into(&mut preferences.model);
        self.set_preferences(preferences, now_ms())
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
            started: true,
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
        started: false,
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

pub(crate) fn delete_session(sessions: &PrivateDir, session: WritableSession) -> SessionDisposal {
    let id = session.metadata.id.clone();
    let named = match sessions.open_child(&id) {
        Ok(Some(named)) => named,
        Ok(None) => return SessionDisposal::Retained,
        Err(_) => return SessionDisposal::Indeterminate,
    };
    match same_directory(&named, &session.owned.dir) {
        Ok(true) => {}
        Ok(false) => return SessionDisposal::Retained,
        Err(_) => return SessionDisposal::Indeterminate,
    }
    drop(named);
    let removed = remove_session_dir(sessions, &id);
    drop(session);
    match removed {
        Ok(()) => SessionDisposal::Discarded,
        Err(_) => SessionDisposal::Indeterminate,
    }
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
    Ok(format!("{STAGING_PREFIX}{}", lowercase_hex(&random)))
}

#[cfg(test)]
mod tests;
