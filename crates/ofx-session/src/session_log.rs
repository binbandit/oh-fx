mod conversation_archive;
mod conversation_history;
mod conversation_progress;
mod conversation_recovery;
mod conversation_writer;
pub(crate) mod managed_file;
mod turn_events;
mod turn_recovery;
mod turn_restore;
mod usage_checkpoint;

use std::fs::File;
use std::mem;
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir, RemoveOutcome};
use ofx_contract::{
    HistoryCut, HistoryTurn, ReasoningEffort, RecoveryPoint, RestoredHistory, TurnEnd, TurnStop,
};
use ofx_text::lowercase_hex;
use ofx_trace::trace_log;

use crate::fx_sessions::{ImportSource, seal, untouched_import};
use crate::history_snapshot::{CacheWriter, HistoryCache};
use crate::session::infer_conversation_language;
use crate::session_children::CONTROL_DIR;
use crate::session_codec::recovery_checkpoint::{RecoveryTranscript, RouteCredential};
use crate::session_codec::{
    MAX_SESSION_METADATA_BYTES, SavedProvider, SessionMetadata, SessionPreferences,
    decode_session_metadata, encode_session_metadata,
};
use crate::session_display_metadata::{derive_display_title, prompt_title};
use crate::session_error::SessionError;
use crate::session_event::{ContextCheckpointEvent, ConversationEvent, ToolResultEvent};
use crate::session_layout::is_valid_session_id;
use crate::session_replay::History;
use crate::session_trace::SESSION;
use crate::session_usage::{Usage, UsageSnapshot};
use crate::session_usage_sidecar;

pub use conversation_archive::{
    ArchivedResult, ArchivedSteering, ArchivedTurn, ExecutedStep, SessionArchive, TurnExecution,
};
pub(crate) use conversation_archive::{check_conversation, load_archive};
pub use conversation_history::{CompactedHistory, SavedHistory, SavedTurn};
use conversation_history::{ReplayScan, replay_history, visit_turns};
pub(crate) use conversation_progress::{ConversationProgress, ProgressPoint};
pub(crate) use conversation_recovery::{
    ConversationRecovery, RecoveredArtifact, RecoveredLog, classify_conversation_recovery,
    converted_log, copy_conversation_recovery_prefix, recovered_log,
};
use conversation_writer::{ConversationWriter, LogScan, scan_log};
use managed_file::{
    Access, create_managed_file, create_private_dir, entry_exists, lock_with_deadline,
    open_managed_file, publish_dir, read_managed_file, remove_dir_tree, same_directory, sync_dir,
};
use turn_events::{TurnArtifacts, turn_events};
pub use turn_recovery::PendingRecovery;
pub(crate) use turn_recovery::{RECOVERY_FILE, read_checkpoint};
use turn_recovery::{
    Recovery, clear_recovery, commit_checkpoint, mark_recovery_asked, open_unfinished_turn,
    recovery_was_asked, save_checkpoint,
};
use turn_restore::{complete_result_output, restored_history};
use usage_checkpoint::UsageCheckpoints;

pub(crate) const EVENTS_FILE: &str = "events.jsonl";
pub(crate) const MANIFEST_FILE: &str = "session.json";
pub(crate) const SESSION_LOCK_FILE: &str = "session.lock";
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
    id: String,
    previous_owner_died: bool,
    _lock: AdvisoryLock,
}

impl OwnedSessionDir {
    fn acquire(dir: PrivateDir, id: &str, lock_deadline: Duration) -> Result<Self, SessionError> {
        let lock = lock_with_deadline(&dir, SESSION_LOCK_FILE, lock_deadline)?
            .ok_or(SessionError::SessionBusy)?;
        Ok(Self::holding(dir, id, lock))
    }

    fn holding(dir: PrivateDir, id: &str, lock: AdvisoryLock) -> Self {
        let previous_owner_died = entry_exists(&dir, OWNER_LIVE_FILE).unwrap_or_else(|error| {
            trace_log!(SESSION, "owner liveness probe failed id={id} err={error}");
            false
        });
        let marker = format!(
            "{{\"pid\":{},\"opened_at_ms\":{}}}\n",
            process::id(),
            now_ms()
        );
        if let Err(error) = dir.replace(OWNER_LIVE_FILE, marker.as_bytes()) {
            trace_log!(SESSION, "owner liveness mark failed id={id} err={error}");
        }
        Self {
            dir,
            id: id.to_owned(),
            previous_owner_died,
            _lock: lock,
        }
    }
}

impl Drop for OwnedSessionDir {
    fn drop(&mut self) {
        let id = &self.id;
        match self.dir.remove(OWNER_LIVE_FILE) {
            Ok(RemoveOutcome::Removed | RemoveOutcome::Missing) => {}
            Ok(RemoveOutcome::RemovedNotDurable) => trace_log!(
                SESSION,
                "owner liveness clear sync failed id={id} err={}",
                DurableError::Failed
            ),
            Err(error) => {
                trace_log!(SESSION, "owner liveness clear failed id={id} err={error}");
            }
        }
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
    usage: Usage,
    usage_checkpoints: UsageCheckpoints,
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
            ofx_trace::trace_log!(
                "session",
                "event=title_generation_apply result=dropped reason=user_title_present"
            );
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
        if let Err(error) = &saved
            && self.writer.turn_open()
            && self.writer.block_open_turn()
        {
            trace_log!(SESSION, "open turn blocked after failed save err={error}");
        }
        saved?;
        if let Err(error) = self.clear_saved_recovery() {
            trace_log!(
                SESSION,
                "event=committed_turn_recovery_cleanup_failed session={} err={error}",
                self.metadata.id
            );
        }
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
            trace_log!(
                SESSION,
                "history committed but language metadata failed session={} err={error}",
                self.metadata.id
            );
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
            Recovery::Saved => match read_checkpoint(&self.owned.dir, self.writer.last_seq()) {
                Ok(Some(checkpoint)) => Box::new(checkpoint),
                Ok(None) => return Ok(()),
                Err(error) => {
                    self.recovery = Recovery::Saved;
                    return Err(error);
                }
            },
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
        clear_recovery(&self.owned.dir).ok();
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
        self.recovery = Recovery::Saved;
        Ok(())
    }

    pub fn discard_recovery(&mut self) {
        self.clear_saved_recovery().ok();
    }

    fn clear_saved_recovery(&mut self) -> Result<(), DurableError> {
        if matches!(self.recovery, Recovery::Absent) {
            return Ok(());
        }
        self.recovery = Recovery::Absent;
        clear_recovery(&self.owned.dir)
    }

    pub fn holds_recovery(&self) -> bool {
        matches!(self.recovery, Recovery::Pending(_) | Recovery::Saved)
    }

    pub fn has_durable_work(&self) -> bool {
        self.writer.last_seq() != 0 || self.holds_recovery()
    }

    pub fn recovery_transcript(&self) -> Option<RecoveryTranscript> {
        match &self.recovery {
            Recovery::Pending(checkpoint) => Some(checkpoint.transcript()),
            Recovery::Absent | Recovery::Continuing | Recovery::Saved => None,
        }
    }

    pub fn recovery_was_asked(&self) -> bool {
        recovery_was_asked(&self.owned.dir)
    }

    pub fn mark_recovery_asked(&self) {
        mark_recovery_asked(&self.owned.dir);
    }

    pub fn settle_open_recovery(&mut self) -> Result<(), SessionError> {
        if self.writer.turn_open() {
            self.settle_recovery()
        } else {
            Ok(())
        }
    }

    pub fn take_authorized_recovery(
        &mut self,
        credential: RouteCredential,
    ) -> Result<PendingRecovery, SessionError> {
        match &self.recovery {
            Recovery::Pending(checkpoint) if !checkpoint.authorizes(credential) => {
                Err(SessionError::RecoveryCredentialAuthorityChanged)
            }
            _ => self.take_recovery().ok_or(SessionError::NoPendingRecovery),
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
        self.write_metadata(proposed).or_else(|error| {
            trace_log!(
                SESSION,
                "derived session title not persisted session={} err={error}",
                self.metadata.id
            );
            self.require_writable()
        })
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
        self.started && !self.has_durable_work() && !self.writer.turn_open()
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
        ultrafast_mode: Option<bool>,
    ) -> Result<(), SessionError> {
        let mut preferences = self.metadata.preferences.clone();
        model.clone_into(&mut preferences.model);
        if let Some(effort) = effort {
            effort.clone_into(&mut preferences.effort);
        }
        preferences.fast_mode = fast_mode;
        if let Some(ultrafast_mode) = ultrafast_mode {
            preferences.ultrafast_mode = ultrafast_mode;
        }
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

    pub(crate) fn seal_import(&self, source: ImportSource) -> Result<(), SessionError> {
        seal(&self.owned.dir, &self.metadata.id, source)
    }

    pub fn rebind_provider(
        &mut self,
        provider: SavedProvider,
        model: &str,
    ) -> Result<(), SessionError> {
        let imported = untouched_import(&self.owned.dir, &self.metadata.id);
        self.select_provider(provider, model)?;
        match imported {
            Some(source) => seal(&self.owned.dir, &self.metadata.id, source),
            None => Ok(()),
        }
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
    let metadata_id = metadata.id.clone();
    let prepared =
        prepare_session(sessions, &staging, &metadata.id, &manifest).map(|(owned, writer)| {
            WritableSession {
                owned,
                writer,
                cache: None,
                metadata,
                history: SavedHistory::default(),
                started: true,
                language,
                recovery: Recovery::Absent,
                work_id: None,
                usage: Usage::fresh(),
                usage_checkpoints: UsageCheckpoints::default(),
            }
        });
    let id = metadata_id;
    let session = match prepared {
        Ok(session) => session,
        Err(error) => {
            discard_unpublished(sessions, &staging, &id);
            return Err(error);
        }
    };
    if let Err(error) = publish_dir(sessions, &staging, &id) {
        if error != SessionError::SessionAlreadyExists {
            trace_log!(
                SESSION,
                "session creation publication failed id={id} err={error}"
            );
        }
        drop(session);
        discard_unpublished(sessions, &staging, &id);
        return Err(error);
    }
    sync_dir(sessions).map_err(|error| {
        trace_log!(
            SESSION,
            "session creation retained published state id={id} durability=uncertain err={error}"
        );
        SessionError::SessionStartFailed
    })?;
    Ok(session)
}

fn discard_unpublished(sessions: &PrivateDir, staging: &str, id: &str) {
    trace_log!(
        SESSION,
        "session creation discarded unpublished state id={id}"
    );
    if let Err(error) = remove_dir_tree(sessions, staging) {
        trace_log!(
            SESSION,
            "session creation cleanup retained id={id} err={error}"
        );
    }
}

fn prepare_session(
    sessions: &PrivateDir,
    staging: &str,
    id: &str,
    manifest: &[u8],
) -> Result<(OwnedSessionDir, ConversationWriter), SessionError> {
    let dir = sessions
        .open_child_private(staging)?
        .ok_or(SessionError::SessionStartFailed)?;
    let owned = OwnedSessionDir::acquire(dir, id, LOCK_DEADLINE)?;
    owned.dir.replace(MANIFEST_FILE, manifest)?;
    let file = create_managed_file(&owned.dir, EVENTS_FILE)?;
    file.sync_all()?;
    session_usage_sidecar::write(&owned.dir, id, &UsageSnapshot::fresh())?;
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
    resume_owned(
        sessions,
        (id, Opening::Resume),
        OwnedSessionDir::acquire(dir, id, lock_deadline)?,
    )
}

pub(crate) fn resume_held_session(
    sessions: &PrivateDir,
    id: &str,
    dir: PrivateDir,
    lock: AdvisoryLock,
) -> Result<WritableSession, SessionError> {
    resume_owned(
        sessions,
        (id, Opening::Import),
        OwnedSessionDir::holding(dir, id, lock),
    )
}

fn resume_owned(
    sessions: &PrivateDir,
    (id, opening): (&str, Opening),
    owned: OwnedSessionDir,
) -> Result<WritableSession, SessionError> {
    let still_named = sessions
        .open_child(id)?
        .is_some_and(|named| same_directory(&named, &owned.dir).unwrap_or(false));
    if !still_named {
        return Err(SessionError::SessionTargetChanged);
    }
    let metadata = read_metadata(&owned.dir, id)?;
    let file = open_managed_file(&owned.dir, EVENTS_FILE, Access::Writable)?
        .ok_or(SessionError::InvalidSessionFormat)?;
    let mut replay = ReplayScan::default();
    let (scan, cache) = scan_writable(&owned.dir, (id, opening), &file, &mut replay)?;
    let mut writer = ConversationWriter::open(file, scan, &mut replay)?;
    let cache = cache.and_then(|cache| cache.finish(&owned.dir, writer.committed_bytes()));
    let closed_from = writer.committed_bytes();
    let recovery =
        open_unfinished_turn(&owned.dir, id, &mut writer, &metadata.preferences.provider)?;
    let end = writer.committed_bytes();
    replay.observe_range(History::log(writer.file()), closed_from, end)?;
    let history = with_history(id, writer.file(), cache.as_ref(), |history| {
        let window = replay.finish(history, end)?;
        replay_history(history, end, &window)
    })?;
    let usage = Usage::restore(
        session_usage_sidecar::load_conversation(&owned.dir, id, metadata.updated_at_ms)?,
        metadata.created_at_ms,
        now_ms(),
    );
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
        usage,
        usage_checkpoints: UsageCheckpoints::default(),
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
    let history = with_history(id, &file, cache.as_ref(), |history| {
        let mut replay = ReplayScan::default();
        let scan = scan_log(history, length, &mut replay, None)?;
        let window = replay.finish(history, scan.complete_bytes)?;
        replay_history(history, scan.complete_bytes, &window)
    })?;
    Ok(SavedSession { metadata, history })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opening {
    Resume,
    Import,
}

fn scan_writable(
    dir: &PrivateDir,
    (id, opening): (&str, Opening),
    file: &File,
    replay: &mut ReplayScan,
) -> Result<(LogScan, Option<CacheWriter>), SessionError> {
    let length = file.metadata()?.len();
    if let Some(mut cache) = CacheWriter::open(dir, id, file, length) {
        let (view, tee) = cache.split();
        let mut cached = ReplayScan::default();
        match scan_log(History::cached(file, view), length, &mut cached, Some(tee)) {
            Ok(scan) => {
                *replay = cached;
                return Ok((scan, Some(cache)));
            }
            Err(error) => trace_log!(
                SESSION,
                "history cache scan rejected id={id} err={error}; rebuilding from log"
            ),
        }
    }
    let mut cache = if length == 0 {
        None
    } else {
        CacheWriter::create(dir, id)
            .inspect_err(|error| match opening {
                Opening::Resume => trace_log!(
                    SESSION,
                    "history cache rebuild-open failed id={id} err={error}"
                ),
                Opening::Import => trace_log!(
                    SESSION,
                    "history cache create-on-import failed id={id} err={error}"
                ),
            })
            .ok()
    };
    let tee = cache.as_mut().map(CacheWriter::tee);
    let scan = scan_log(History::log(file), length, replay, tee)?;
    Ok((scan, cache))
}

fn with_history<T>(
    id: &str,
    file: &File,
    cache: Option<&HistoryCache>,
    read: impl Fn(History<'_>) -> Result<T, SessionError>,
) -> Result<T, SessionError> {
    if let Some(cache) = cache {
        match read(History::cached(file, cache.view())) {
            Ok(value) => return Ok(value),
            Err(error) => trace_log!(
                SESSION,
                "history cache replay rejected id={id} err={error}; reading log"
            ),
        }
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

pub(crate) fn delete_session(
    sessions: &PrivateDir,
    session: WritableSession,
    event: &str,
) -> SessionDisposal {
    let id = session.metadata.id.clone();
    let owned = sessions
        .open_child(&id)
        .map_err(SessionError::from)
        .and_then(|named| match named {
            Some(named) => same_directory(&named, &session.owned.dir),
            None => Ok(false),
        });
    match owned {
        Ok(true) => {}
        Ok(false) => {
            trace_log!(
                SESSION,
                "event={event} disposition=retained reason=store_root_mismatch"
            );
            return SessionDisposal::Retained;
        }
        Err(error) => {
            trace_log!(
                SESSION,
                "event={event} disposition=retained reason=store_root_unverified err={error}"
            );
            return SessionDisposal::Retained;
        }
    }
    let removed = remove_dir_tree(sessions, &id);
    let (stage, error) = match removed {
        Err(error) => ("delete", error),
        Ok(()) => match sync_dir(sessions) {
            Ok(()) => {
                session.forget_usage_recovery();
                drop(session);
                trace_log!(SESSION, "event={event} disposition=discarded");
                return SessionDisposal::Discarded;
            }
            Err(error) => ("sync", error),
        },
    };
    drop(session);
    trace_log!(
        SESSION,
        "event={event} disposition=indeterminate stage={stage} err={error}"
    );
    SessionDisposal::Indeterminate
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

pub(crate) fn staging_name() -> Result<String, SessionError> {
    let mut random = [0_u8; STAGING_RANDOM_BYTES];
    getrandom::fill(&mut random).map_err(|_| SessionError::SessionStartFailed)?;
    Ok(format!("{STAGING_PREFIX}{}", lowercase_hex(&random)))
}

#[cfg(test)]
mod tests;
