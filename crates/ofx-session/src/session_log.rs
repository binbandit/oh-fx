mod conversation_history;
mod conversation_progress;
mod conversation_writer;
pub(crate) mod managed_file;
mod turn_events;
mod turn_recovery;
mod turn_restore;

use std::fs::File;
use std::mem;
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir};
use ofx_contract::{
    HistoryCut, HistoryTurn, ReasoningEffort, RecoveryPoint, RestoredHistory, TurnEnd, TurnStop,
};
use ofx_text::lowercase_hex;

use crate::history_snapshot::{CacheWriter, HistoryCache};
use crate::session::infer_conversation_language;
use crate::session_children::CONTROL_DIR;
use crate::session_codec::recovery_checkpoint::RouteCredential;
use crate::session_codec::{
    MAX_SESSION_METADATA_BYTES, SavedProvider, SessionMetadata, SessionPreferences,
    decode_session_metadata, encode_session_metadata,
};
use crate::session_display_metadata::{derive_display_title, prompt_title};
use crate::session_error::SessionError;
use crate::session_event::{ContextCheckpointEvent, ConversationEvent, ToolResultEvent};
use crate::session_layout::is_valid_session_id;
use crate::session_replay::History;

pub use conversation_history::{CompactedHistory, SavedHistory, SavedTurn};
use conversation_history::{ReplayScan, replay_history, visit_turns};
use conversation_progress::ProgressPoint;
use conversation_writer::{ConversationWriter, LogScan, scan_log};
use managed_file::{
    Access, create_managed_file, create_private_dir, entry_exists, lock_with_deadline,
    open_managed_file, publish_dir, read_managed_file, remove_created_dir, remove_session_dir,
    same_directory, sync_dir,
};
use turn_events::{TurnArtifacts, turn_events};
pub use turn_recovery::PendingRecovery;
use turn_recovery::{
    Recovery, clear_recovery, commit_checkpoint, open_unfinished_turn, save_checkpoint,
};
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
    cache: Option<HistoryCache>,
    metadata: SessionMetadata,
    history: SavedHistory,
    started: bool,
    language: String,
    recovery: Recovery,
    work_id: Option<String>,
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
        visit_turns(self.history(), self.writer.committed_bytes(), visit)
    }

    fn history(&self) -> History<'_> {
        match &self.cache {
            Some(cache) => History::cached(self.writer.file(), cache.view()),
            None => History::log(self.writer.file()),
        }
    }

    pub fn begin_work(&mut self, work_id: &str) {
        self.work_id = Some(work_id.to_owned());
    }

    pub(crate) fn control_dir(&self) -> Result<PrivateDir, SessionError> {
        Ok(self.owned.dir.open_or_create_child(CONTROL_DIR)?)
    }

    pub fn observe_prompt(&mut self, prompt: &str) {
        let inferred = infer_conversation_language(prompt, &self.language);
        if inferred != self.language {
            self.language = inferred.to_owned();
        }
    }

    pub fn display_title(&self) -> String {
        match &self.metadata.title {
            Some(title) => title.clone(),
            None => derive_display_title(&self.history),
        }
    }

    pub fn title(&self) -> Option<&str> {
        self.metadata.title.as_deref()
    }

    pub fn rename(&mut self, title: &str) -> Result<(), SessionError> {
        let mut proposed = self.metadata.clone();
        proposed.title = Some(title.to_owned());
        self.write_metadata(proposed)
    }

    pub fn install_generated_title(&mut self, title: &str) -> Result<bool, SessionError> {
        if let Some(persisted) = &self.metadata.title
            && self.derived_title()?.as_ref() != Some(persisted)
        {
            return Ok(false);
        }
        self.rename(title)?;
        Ok(true)
    }

    fn derived_title(&self) -> Result<Option<String>, SessionError> {
        let mut prompts = SavedHistory::default();
        self.visit_transcript(|turn| {
            prompts.turns.push(SavedTurn {
                events: turn.events.into_iter().take(1).collect(),
            });
        })?;
        Ok((!prompts.turns.is_empty()).then(|| derive_display_title(&prompts)))
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
        let fresh = self.started;
        let nothing_done = turn.steps.is_empty()
            && turn.steering.is_empty()
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
        saved?;
        self.discard_recovery();
        self.write_first_title(fresh, turn.user)?;
        self.save_language()
    }

    fn save_language(&mut self) -> Result<(), SessionError> {
        if self.language == self.metadata.conversation_language {
            return Ok(());
        }
        let mut proposed = self.metadata.clone();
        proposed.conversation_language.clone_from(&self.language);
        self.write_metadata(proposed).map_err(|error| {
            self.writer.mark_uncertain();
            match error {
                SessionError::SessionPersistenceUncertain => error,
                _ => SessionError::SessionPersistenceUncertain,
            }
        })
    }

    pub fn settle_recovery(&mut self) -> Result<(), SessionError> {
        let checkpoint = match mem::take(&mut self.recovery) {
            Recovery::Pending(checkpoint) => checkpoint,
            other => {
                self.recovery = other;
                return Ok(());
            }
        };
        commit_checkpoint(
            &self.owned.dir,
            &mut self.writer,
            &self.metadata.preferences.provider,
            checkpoint,
        )?;
        self.history = replayed_history(self.history(), self.writer.committed_bytes())?;
        Ok(())
    }

    pub fn record_recovery(
        &mut self,
        point: &RecoveryPoint<'_>,
        provider: &SavedProvider,
        credential: RouteCredential,
    ) -> Result<(), SessionError> {
        self.require_writable()?;
        save_checkpoint(
            &self.owned.dir,
            self.writer.last_seq(),
            point,
            provider,
            credential,
        )?;
        if !matches!(self.recovery, Recovery::Continuing) {
            self.recovery = Recovery::Saved;
        }
        Ok(())
    }

    pub fn discard_recovery(&mut self) {
        if !matches!(self.recovery, Recovery::Absent) {
            clear_recovery(&self.owned.dir);
            self.recovery = Recovery::Absent;
        }
    }

    pub fn settle_open_recovery(&mut self) -> Result<(), SessionError> {
        if self.writer.turn_open() {
            self.settle_recovery()
        } else {
            Ok(())
        }
    }

    pub fn take_recovery(&mut self) -> Option<PendingRecovery> {
        match mem::take(&mut self.recovery) {
            Recovery::Pending(checkpoint) => {
                self.recovery = Recovery::Continuing;
                Some(PendingRecovery::new(checkpoint, &self.owned.dir))
            }
            other => {
                self.recovery = other;
                None
            }
        }
    }

    fn append_turn(
        &mut self,
        turn: &HistoryTurn<'_>,
        provider: &SavedProvider,
        open: bool,
    ) -> Result<(), SessionError> {
        let timestamp_ms = now_ms();
        let written = self.written()?;
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
        let fresh = self.started;
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
        self.append(timestamp_ms, &events)?;
        match active {
            Some(active) => self.write_first_title(fresh, active.user),
            None => Ok(()),
        }
    }

    fn write_first_title(&mut self, fresh: bool, prompt: &str) -> Result<(), SessionError> {
        let untitled = fresh && self.metadata.title.is_none();
        let Some(title) = untitled.then(|| prompt_title(prompt)).flatten() else {
            return Ok(());
        };
        let mut proposed = self.metadata.clone();
        proposed.title = Some(title);
        self.write_metadata(proposed)
            .or_else(|_| self.require_writable())
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
        let written = self.written()?;
        let mut events = turn_events(&self.artifacts(provider, timestamp_ms), active, written)?;
        events.pop();
        events.drain(..usize::from(self.writer.turn_open()));
        Ok(events)
    }

    pub(crate) fn is_pristine(&self) -> bool {
        self.started && self.writer.last_seq() == 0 && !self.writer.turn_open()
    }

    fn written(&self) -> Result<ProgressPoint, SessionError> {
        if !self.writer.turn_open() {
            return Ok(ProgressPoint::default());
        }
        Ok(self.writer.context_progress(None)?.point)
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
            work_id: self.work_id.as_deref(),
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
            self.started = false;
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
    let language = metadata.conversation_language.clone();
    let prepared =
        prepare_session(sessions, &staging, &manifest).map(|(owned, writer)| WritableSession {
            owned,
            writer,
            cache: None,
            metadata,
            history: SavedHistory::default(),
            started: true,
            language,
            recovery: Recovery::Absent,
            work_id: None,
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
    let (scan, cache) = scan_writable(&owned.dir, id, &file, &mut replay)?;
    let mut writer = ConversationWriter::open(file, scan, &mut replay)?;
    let cache = cache.and_then(|cache| cache.finish(&owned.dir, writer.committed_bytes()));
    let closed_from = writer.committed_bytes();
    let recovery = open_unfinished_turn(&owned.dir, &mut writer)?;
    let end = writer.committed_bytes();
    replay.observe_range(History::log(writer.file()), closed_from, end)?;
    let history = with_history(writer.file(), cache.as_ref(), |history| {
        let window = replay.finish(history, end)?;
        replay_history(history, end, &window)
    })?;
    Ok(WritableSession {
        owned,
        writer,
        cache,
        language: metadata.conversation_language.clone(),
        metadata,
        history,
        started: false,
        recovery,
        work_id: None,
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
    let cache = HistoryCache::open(&dir, id, &file, length);
    let history = with_history(&file, cache.as_ref(), |history| {
        let mut replay = ReplayScan::default();
        let scan = scan_log(history, length, &mut replay, None)?;
        let window = replay.finish(history, scan.complete_bytes)?;
        replay_history(history, scan.complete_bytes, &window)
    })?;
    Ok(SavedSession { metadata, history })
}

fn scan_writable(
    dir: &PrivateDir,
    id: &str,
    file: &File,
    replay: &mut ReplayScan,
) -> Result<(LogScan, Option<CacheWriter>), SessionError> {
    let length = file.metadata()?.len();
    if let Some(mut cache) = CacheWriter::open(dir, id, file, length) {
        let (view, tee) = cache.split();
        let mut cached = ReplayScan::default();
        if let Ok(scan) = scan_log(History::cached(file, view), length, &mut cached, Some(tee)) {
            *replay = cached;
            return Ok((scan, Some(cache)));
        }
    }
    let mut cache = if length == 0 {
        None
    } else {
        CacheWriter::create(dir, id)
    };
    let tee = cache.as_mut().map(CacheWriter::tee);
    let scan = scan_log(History::log(file), length, replay, tee)?;
    Ok((scan, cache))
}

fn with_history<T>(
    file: &File,
    cache: Option<&HistoryCache>,
    read: impl Fn(History<'_>) -> Result<T, SessionError>,
) -> Result<T, SessionError> {
    if let Some(cache) = cache
        && let Ok(value) = read(History::cached(file, cache.view()))
    {
        return Ok(value);
    }
    read(History::log(file))
}

pub(crate) fn work_reply(
    sessions: &PrivateDir,
    id: &str,
    work_id: &str,
) -> Result<Option<String>, SessionError> {
    if !is_valid_session_id(id) {
        return Err(SessionError::InvalidSessionId);
    }
    let dir = sessions
        .open_child(id)?
        .ok_or(SessionError::SessionNotFound)?;
    read_metadata(&dir, id)?;
    let file = open_managed_file(&dir, EVENTS_FILE, Access::ReadOnly)?
        .ok_or(SessionError::InvalidSessionFormat)?;
    let length = file.metadata()?.len();
    let scan = scan_log(
        History::log(&file),
        length,
        &mut ReplayScan::default(),
        None,
    )?;
    let mut reply = None;
    visit_turns(History::log(&file), scan.complete_bytes, |turn| {
        if let Some(text) = turn.reply_for_work(work_id) {
            reply = Some(text);
        }
    })?;
    Ok(reply)
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

fn replayed_history(history: History<'_>, end: u64) -> Result<SavedHistory, SessionError> {
    let mut replay = ReplayScan::default();
    scan_log(history, end, &mut replay, None)?;
    let window = replay.finish(history, end)?;
    replay_history(history, end, &window)
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
