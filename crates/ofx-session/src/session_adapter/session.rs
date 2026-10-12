use std::collections::HashSet;
use std::path::Path;
use std::process;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ofx_contract::{
    ConversationLog, DeliveryOutcome, FileChangeStats, HistoryCut, HistoryTurn, LogFailure,
    ProviderBilling, RecoveryPoint, RequestTicket, TurnEnd, TurnStop,
};
use ofx_text::lowercase_hex;
use ofx_trace::trace_log;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::session::infer_conversation_language;
use crate::session_codec::{
    DEFAULT_CONVERSATION_LANGUAGE, SavedProvider, SessionPreferences, effort_label,
};
use crate::session_display_metadata::prompt_title;
use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, InterruptReason, encode_event_payload};
use crate::session_log::{ProgressPoint, ResultSink, TurnArtifacts, turn_events};
use crate::session_manager::{AppendError, Event, Host, Reason, SetKey, WritingSession};
use crate::session_trace::SESSION;

const WIRING: &str = "wiring";
const EMPTY_PERMISSIONS: &[u8] = b"{\"schema_version\":2,\"next_generation\":1,\"rules\":[]}";
const RESULT_PREFIX: &str = "result-";
const RESULT_SUFFIX: &str = ".txt";

#[derive(Serialize)]
struct SavedPreferences<'a> {
    provider: &'a SavedProvider,
    model: &'a str,
    effort: &'a str,
    fast_mode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    ultrafast_mode: Option<bool>,
}

struct Writer {
    session: WritingSession,
    provider: SavedProvider,
    language: String,
    turn_open: bool,
    last_turn: u64,
    pending: Vec<String>,
    fresh: bool,
    titled: bool,
    requests: u64,
}

pub struct SessionV2 {
    writer: Arc<Mutex<Writer>>,
    id: String,
}

struct SessionV2Log {
    writer: Arc<Mutex<Writer>>,
}

struct StoredResults(HashSet<String>);

impl ResultSink for StoredResults {
    fn store(&self, _: &str, _: &str, output: &str) -> Result<String, SessionError> {
        let hash = blob_hash(output.as_bytes());
        if !self.0.contains(&hash) {
            return Err(SessionError::InvalidConversationEvent);
        }
        Ok(format!("{RESULT_PREFIX}{hash}{RESULT_SUFFIX}"))
    }
}

enum CommitError {
    Write(AppendError),
    Encode(SessionError),
}

impl From<AppendError> for CommitError {
    fn from(error: AppendError) -> Self {
        Self::Write(error)
    }
}

impl From<SessionError> for CommitError {
    fn from(error: SessionError) -> Self {
        Self::Encode(error)
    }
}

impl SessionV2 {
    pub(super) fn create(
        root: &Path,
        workspace: &str,
        host: Host,
        preferences: &SessionPreferences,
        prompt: &str,
    ) -> Result<Self, SessionError> {
        let session =
            WritingSession::open_new(root, workspace, host).map_err(|error| match error {
                AppendError::InvalidArgument => SessionError::InvalidWorkspaceRoot,
                _ => SessionError::SessionStoreUnavailable,
            })?;
        let language =
            infer_conversation_language(prompt, DEFAULT_CONVERSATION_LANGUAGE).to_owned();
        let mut writer = Writer {
            session,
            provider: preferences.provider.clone(),
            language,
            turn_open: false,
            last_turn: 0,
            pending: Vec::new(),
            fresh: true,
            titled: false,
            requests: 0,
        };
        writer
            .append_seed(preferences)
            .map_err(|_| SessionError::SessionStoreUnavailable)?;
        let id = writer.session.id().to_owned();
        trace_log!(WIRING, "action=Open session={id} pid={}", process::id());
        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            id,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn log(&self) -> Box<dyn ConversationLog> {
        Box::new(SessionV2Log {
            writer: Arc::clone(&self.writer),
        })
    }

    pub fn close(self) -> String {
        let mut writer = lock(&self.writer);
        if let Err(error) = writer.session.close() {
            trace_log!(
                SESSION,
                "event=sessions_v2_close_failed session={} err={error}",
                self.id
            );
        }
        trace_log!(
            WIRING,
            "action=Close session={} pid={}",
            self.id,
            process::id()
        );
        if writer.turn_open {
            trace_log!(
                SESSION,
                "event=sessions_v2_turn_closed_open session={} streamed=0",
                self.id
            );
        }
        if writer.last_turn > 0 {
            self.id.clone()
        } else {
            String::new()
        }
    }
}

impl Writer {
    fn append_seed(&mut self, preferences: &SessionPreferences) -> Result<(), AppendError> {
        let saved = serde_json::to_vec(&SavedPreferences {
            provider: &preferences.provider,
            model: &preferences.model,
            effort: effort_label(&preferences.effort),
            fast_mode: preferences.fast_mode,
            ultrafast_mode: preferences.ultrafast_mode.then_some(true),
        })
        .map_err(|_| AppendError::InvalidArgument)?;
        let language = json_string(&self.language);
        self.write(&[
            Event::Set(SetKey::Prefs, &saved),
            Event::Set(SetKey::Permissions, EMPTY_PERMISSIONS),
            Event::Set(SetKey::Language, &language),
        ])
    }

    fn write(&mut self, events: &[Event<'_>]) -> Result<(), AppendError> {
        self.session.append(events)?;
        let id = self.session.id();
        let pid = process::id();
        for event in events {
            match event {
                Event::TurnStarted => {
                    trace_log!(WIRING, "action=BeginTurn session={id} pid={pid}");
                }
                Event::TurnCommitted => {
                    trace_log!(WIRING, "action=EndTurn session={id} end=commit pid={pid}");
                }
                Event::TurnInterrupted(_) => {
                    trace_log!(
                        WIRING,
                        "action=EndTurn session={id} end=interrupt pid={pid}"
                    );
                }
                Event::Item { .. } | Event::Set(..) => {}
            }
        }
        Ok(())
    }

    fn commit_turn(&mut self, turn: &HistoryTurn<'_>) -> Result<(), CommitError> {
        let nothing_done = turn.steps.is_empty()
            && turn.steering.is_empty()
            && turn.end
                == TurnEnd::Stopped {
                    reason: TurnStop::Failed,
                    partial: "",
                };
        if nothing_done && !self.turn_open {
            return Ok(());
        }
        let outputs: Vec<&str> = turn
            .steps
            .iter()
            .flat_map(|step| step.tool_results.iter().map(|result| result.output))
            .collect();
        if !outputs.is_empty() && !self.turn_open {
            self.write(&[Event::TurnStarted])?;
            self.started_turn();
        }
        let mut stored = HashSet::new();
        for output in outputs {
            let hash = self.session.put_blob(output.as_bytes())?;
            if !self.pending.contains(&hash) {
                self.pending.push(hash.clone());
            }
            stored.insert(hash);
        }
        let artifacts = TurnArtifacts {
            results: &StoredResults(stored),
            provider: &self.provider,
            timestamp_ms: now_ms(),
            work_id: None,
        };
        let events = turn_events(&artifacts, turn, ProgressPoint::default())?;
        let pieces = events
            .iter()
            .map(|event| Ok((item_type(event)?, encode_event_payload(event)?)))
            .collect::<Result<Vec<_>, SessionError>>()?;
        let end = match events.last() {
            Some(ConversationEvent::TurnCompleted(_)) => Event::TurnCommitted,
            Some(ConversationEvent::Interrupted(interrupted)) => {
                Event::TurnInterrupted(match interrupted.reason {
                    InterruptReason::Cancelled => Reason::Cancel,
                    InterruptReason::Failed => Reason::Failed,
                })
            }
            _ => return Err(SessionError::InvalidConversationFrame.into()),
        };
        let title = (self.fresh && !self.titled)
            .then(|| prompt_title(turn.user))
            .flatten()
            .map(|title| json_string(&title));
        let pending = self.pending.clone();
        let was_open = self.turn_open;
        let mut batch = Vec::with_capacity(pieces.len() + 4);
        if !was_open {
            batch.push(Event::TurnStarted);
        }
        for (at, (item_type, data)) in pieces.iter().enumerate() {
            batch.push(Event::Item {
                item_type,
                data,
                blobs: if at == 0 { &pending } else { &[] },
            });
        }
        batch.push(end);
        if let Some(title) = &title {
            batch.push(Event::Set(SetKey::Title, title));
        }
        self.write(&batch)?;
        if !was_open {
            self.started_turn();
        }
        self.turn_open = false;
        self.pending.clear();
        self.titled |= title.is_some();
        self.fresh = false;
        Ok(())
    }

    fn started_turn(&mut self) {
        self.last_turn += 1;
        self.turn_open = true;
    }
}

impl ConversationLog for SessionV2Log {
    fn require_writable(&self) -> Result<(), LogFailure> {
        lock(&self.writer)
            .session
            .failure()
            .map_or(Ok(()), |error| Err(log_failure(&error.to_string())))
    }

    fn record_turn(&mut self, turn: &HistoryTurn<'_>) -> Result<(), LogFailure> {
        lock(&self.writer)
            .commit_turn(turn)
            .map_err(|error| match error {
                CommitError::Write(error) => log_failure(&error.to_string()),
                CommitError::Encode(error) => log_failure(&error.to_string()),
            })
    }

    fn record_compaction(
        &mut self,
        _: &str,
        _: HistoryCut,
        _: Option<&HistoryTurn<'_>>,
    ) -> Result<(), LogFailure> {
        Ok(())
    }

    fn record_recovery(&self, _: &RecoveryPoint<'_>) -> Result<(), LogFailure> {
        Ok(())
    }

    fn keeps_recovery(&self) -> bool {
        false
    }

    fn clear_recovery(&self) -> Result<(), LogFailure> {
        Ok(())
    }

    fn begin_request(&self) -> Result<RequestTicket, LogFailure> {
        let mut writer = lock(&self.writer);
        writer.requests += 1;
        Ok(RequestTicket {
            sequence: writer.requests,
            started_at: Instant::now(),
        })
    }

    fn finish_request(&self, _: RequestTicket, _: DeliveryOutcome) -> Result<(), LogFailure> {
        Ok(())
    }

    fn finish_exact_request(
        &self,
        _: RequestTicket,
        _: &ProviderBilling,
    ) -> Result<(), LogFailure> {
        Ok(())
    }

    fn record_committed_lines(&self, _: FileChangeStats) {}
}

fn item_type(event: &ConversationEvent) -> Result<&'static str, SessionError> {
    Ok(match event {
        ConversationEvent::User(_) => "user",
        ConversationEvent::Assistant(_) => "assistant",
        ConversationEvent::ToolCall(_) => "tool_call",
        ConversationEvent::ToolResult(_) => "tool_result",
        ConversationEvent::Steering(_) => "steering",
        ConversationEvent::TurnCompleted(_) => "turn_end",
        ConversationEvent::Interrupted(_) => "interruption",
        ConversationEvent::ContextCheckpoint(_) => {
            return Err(SessionError::InvalidConversationFrame);
        }
    })
}

fn json_string(text: &str) -> Vec<u8> {
    serde_json::to_vec(text).unwrap_or_default()
}

fn blob_hash(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes))
}

fn lock(writer: &Mutex<Writer>) -> MutexGuard<'_, Writer> {
    writer.lock().unwrap_or_else(PoisonError::into_inner)
}

fn log_failure(code: &str) -> LogFailure {
    LogFailure {
        code: code.to_owned(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
