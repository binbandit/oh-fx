use sha2::{Digest, Sha256};

use super::LegacySession;
use super::durable_state::{DurableState, durable_state};
use super::durable_turn::LegacyTurn;
use super::legacy_checkpoint::LegacyCheckpoint;
use super::legacy_frame::{
    Chunk, Envelope, Event, PreferenceChange, RAW_CHUNK_BYTES, Replacement, Started,
};
use crate::session_authority::Identifier;
use crate::session_error::SessionError;

pub(super) struct Replay {
    pub(super) session: LegacySession,
    pub(super) generation: Identifier,
    pub(super) seq: u64,
    pub(super) event_id: Identifier,
    pub(super) recovery: Option<LegacyCheckpoint>,
    pending: Option<Pending>,
}

struct Pending {
    start: Replacement,
    chunks: u64,
    bytes: Vec<u8>,
}

impl Replay {
    pub(super) fn start(envelope: Envelope, id: &str) -> Result<Self, SessionError> {
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
            recovery: None,
            pending: None,
        })
    }

    pub(super) fn settled(&self) -> bool {
        self.pending.is_none()
    }

    pub(super) fn apply(&mut self, envelope: Envelope) -> Result<(), SessionError> {
        if envelope.generation != self.generation || Some(envelope.seq) != self.seq.checked_add(1) {
            return Err(SessionError::InvalidSessionFormat);
        }
        let timestamp_ms = envelope.timestamp_ms;
        match (self.pending.take(), envelope.event) {
            (Some(mut pending), Event::ReplacementChunk(chunk)) => {
                pending.add(chunk)?;
                self.pending = Some(pending);
            }
            (Some(pending), Event::ReplacementCommitted(commit)) => {
                self.replace(&pending, commit, timestamp_ms)?;
            }
            (None, Event::ReplacementStarted(start)) => self.pending = Some(Pending::new(start)?),
            (Some(_), _)
            | (
                None,
                Event::Started(_) | Event::ReplacementChunk(_) | Event::ReplacementCommitted(_),
            ) => return Err(SessionError::InvalidSessionFormat),
            (None, event) => self.reduce(event, timestamp_ms)?,
        }
        self.seq = envelope.seq;
        self.event_id = envelope.event_id;
        Ok(())
    }

    fn reduce(&mut self, event: Event, timestamp_ms: i64) -> Result<(), SessionError> {
        let session = &mut self.session;
        match event {
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
                if matches!(turn, LegacyTurn::Compacted(_)) {
                    session.context_history_start = session.turns.len();
                }
                session.turns.push(turn);
                session.conversation_language = conversation_language;
                self.recovery = None;
            }
            Event::RecoverySet(checkpoint) => self.recovery = Some(checkpoint),
            Event::RecoveryCleared => self.recovery = None,
            Event::UsageCheckpointed(usage) => session.usage = Some(*usage),
            Event::Started(_)
            | Event::ReplacementStarted(_)
            | Event::ReplacementChunk(_)
            | Event::ReplacementCommitted(_) => {}
        }
        session.updated_at_ms = timestamp_ms;
        Ok(())
    }

    fn replace(
        &mut self,
        pending: &Pending,
        commit: Replacement,
        timestamp_ms: i64,
    ) -> Result<(), SessionError> {
        let start = pending.start;
        let complete = commit.id == start.id
            && commit.encoded_bytes == start.encoded_bytes
            && commit.sha256 == start.sha256
            && commit.chunk_count == start.chunk_count
            && pending.chunks == start.chunk_count
            && u64::try_from(pending.bytes.len()).ok() == Some(start.encoded_bytes)
            && Sha256::digest(&pending.bytes).as_slice() == start.sha256;
        if !complete {
            return Err(SessionError::InvalidSessionFormat);
        }
        let mut state = durable_state(&pending.bytes).ok_or(SessionError::InvalidSessionFormat)?;
        let prior = &self.session;
        let same_session = state.id == prior.id
            && state.created_at_ms == prior.created_at_ms
            && state.origin_workspace_root == prior.origin_workspace_root
            && state.workspace_root == prior.workspace_root
            && state.updated_at_ms == timestamp_ms
            && (!start.log_rewrite || timestamp_ms == prior.updated_at_ms);
        if !same_session {
            return Err(SessionError::InvalidSessionFormat);
        }
        self.recovery = state.recovery.take();
        self.session = LegacySession::replaced(state);
        Ok(())
    }
}

impl Pending {
    fn new(start: Replacement) -> Result<Self, SessionError> {
        if start.chunk_count != start.encoded_bytes.div_ceil(RAW_CHUNK_BYTES) {
            return Err(SessionError::InvalidSessionFormat);
        }
        Ok(Self {
            start,
            chunks: 0,
            bytes: Vec::new(),
        })
    }

    fn add(&mut self, chunk: Chunk) -> Result<(), SessionError> {
        let read =
            u64::try_from(self.bytes.len()).map_err(|_| SessionError::InvalidSessionFormat)?;
        let expected = if self.chunks + 1 == self.start.chunk_count {
            self.start.encoded_bytes.saturating_sub(read)
        } else {
            RAW_CHUNK_BYTES
        };
        let fits = chunk.replacement == self.start.id
            && chunk.index == self.chunks
            && self.chunks < self.start.chunk_count
            && u64::try_from(chunk.bytes.len()).ok() == Some(expected);
        if !fits {
            return Err(SessionError::InvalidSessionFormat);
        }
        self.bytes.extend(chunk.bytes);
        self.chunks += 1;
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
            context_history_start: 0,
            recovery: None,
            usage: started.usage.map(|usage| *usage),
        }
    }

    fn replaced(state: DurableState) -> Self {
        Self {
            id: state.id,
            origin_workspace_root: state.origin_workspace_root,
            workspace_root: state.workspace_root,
            created_at_ms: state.created_at_ms,
            updated_at_ms: state.updated_at_ms,
            conversation_language: state.conversation_language,
            preferences: state.preferences,
            subagent_child: state.subagent_child,
            turns: state.turns,
            context_history_start: state.context_history_start,
            recovery: None,
            usage: state.usage,
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
