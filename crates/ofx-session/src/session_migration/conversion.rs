use std::io::Write as _;

use ofx_config::PrivateDir;

use super::LegacySession;
use super::durable_turn::{
    ConversationTurn, Execution, LegacyTurn, SavedResult, Steering, TurnClose,
};
use super::legacy_presentation::{CommandReplay, LegacyPresentation};
use super::recovery_file::recovery_file;
use crate::result_store::{
    PREVIEW_BYTES, bytes_handle, bytes_preview, diff_content_pack, fits_diff_pack,
    store_new_results,
};
use crate::session_codec::{SessionMetadata, encode_session_metadata};
use crate::session_display_metadata::history_title;
use crate::session_error::SessionError;
use crate::session_event::{
    ArtifactCompleteness, AssistantEvent, CommittedFilePresentation, ContextCheckpointEvent,
    ConversationEvent, ConversationState, FileEvidence, InterruptedEvent, SavedReplay,
    SteeringEvent, ToolResultEvent, TurnCompletedEvent, UserEvent, encode_conversation_frame,
};
use crate::session_log::managed_file::{create_managed_file, sync_dir};
use crate::session_log::{
    ConversationProgress, EVENTS_FILE, MANIFEST_FILE, ProgressPoint, RECOVERY_FILE,
};
use crate::session_summary_codec::{SessionSource, SessionSummary};
use crate::session_usage::UsageSnapshot;
use crate::session_usage_sidecar;

const LISTED_OUTPUT: &str = "result-listed.txt";
const LISTED_PACK: &str = "diff-listed.json";

pub(crate) struct Converted {
    metadata: SessionMetadata,
    events: Vec<ConversationEvent>,
    history_len: usize,
    results: Vec<StoredResult>,
    recovery: Option<Vec<u8>>,
    usage: Option<UsageSnapshot>,
    source_bytes: u64,
}

struct LogBuilder {
    state: ConversationState,
    events: Vec<ConversationEvent>,
    timestamp_ms: i64,
}

struct StoredResult {
    handle: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    Listing,
    Import,
}

struct Results {
    purpose: Purpose,
    stored: Vec<StoredResult>,
}

impl Results {
    fn output(&mut self, call_id: &str, tool_name: &str, bytes: Vec<u8>) -> String {
        if self.purpose == Purpose::Listing {
            return LISTED_OUTPUT.to_owned();
        }
        let handle = bytes_handle(call_id, tool_name, &bytes);
        self.stored.push(StoredResult {
            handle: handle.clone(),
            bytes,
        });
        handle
    }

    fn pack(
        &mut self,
        call_id: &str,
        previous: Option<&[u8]>,
        after: Option<&[u8]>,
    ) -> Option<String> {
        if self.purpose == Purpose::Listing {
            return Some(LISTED_PACK.to_owned());
        }
        let (handle, bytes) = diff_content_pack(call_id, previous, after)?;
        self.stored.push(StoredResult {
            handle: handle.clone(),
            bytes,
        });
        Some(handle)
    }
}

impl LegacySession {
    pub(super) fn convert(
        self,
        purpose: Purpose,
        source_bytes: u64,
    ) -> Result<Converted, SessionError> {
        let history_len = self.turns.len();
        let prompts = self.turns.iter().filter_map(|turn| match turn {
            LegacyTurn::Conversation(turn) => Some(turn.user.as_str()),
            LegacyTurn::Compacted(_) => None,
        });
        let metadata = SessionMetadata {
            title: history_title(history_len, prompts),
            id: self.id,
            origin_workspace_root: self.origin_workspace_root,
            workspace_root: self.workspace_root,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            conversation_language: self.conversation_language,
            preferences: self.preferences,
            subagent_child: false,
        };
        encode_session_metadata(&metadata)?;
        let mut results = Results {
            purpose,
            stored: Vec::new(),
        };
        let mut log = LogBuilder {
            state: ConversationState::default(),
            events: Vec::new(),
            timestamp_ms: metadata.updated_at_ms,
        };
        for (index, turn) in self.turns.into_iter().enumerate() {
            let batch = match turn {
                LegacyTurn::Compacted(compacted) => {
                    let active = index > 0 && index == self.context_history_start;
                    let covers_through_seq = if active {
                        log.coverage(compacted.removed_turn_count)?
                    } else {
                        log.state.latest_checkpoint_coverage()
                    };
                    vec![ConversationEvent::ContextCheckpoint(
                        ContextCheckpointEvent {
                            covers_through_seq,
                            summary: compacted.summary,
                        },
                    )]
                }
                LegacyTurn::Conversation(turn) => turn_events(*turn, &mut results)?,
            };
            log.append(batch)?;
        }
        let seq = u64::try_from(log.events.len())
            .map_err(|_| SessionError::ConversationSequenceOverflow)?;
        let file = match &self.recovery {
            Some(checkpoint) => recovery_file(checkpoint, seq)?,
            None => None,
        };
        let recovery = file.map(|file| {
            results
                .stored
                .extend(file.spilled.into_iter().map(|(handle, text)| StoredResult {
                    handle,
                    bytes: text.into_bytes(),
                }));
            file.bytes
        });
        Ok(Converted {
            metadata,
            events: log.events,
            history_len,
            results: results.stored,
            recovery,
            usage: self.usage,
            source_bytes,
        })
    }
}

impl LogBuilder {
    fn append(&mut self, batch: Vec<ConversationEvent>) -> Result<(), SessionError> {
        for event in &batch {
            let seq = self.state.next_seq()?;
            self.state.apply(seq, self.timestamp_ms, event)?;
        }
        if self.state.has_pending_tool_calls() {
            return Err(SessionError::UnresolvedToolCall);
        }
        self.events.extend(batch);
        Ok(())
    }

    fn coverage(&self, turns: usize) -> Result<u64, SessionError> {
        let latest = self.state.latest_checkpoint_coverage();
        let cut = ProgressPoint {
            turns,
            ..ProgressPoint::default()
        };
        let mut progress = ConversationProgress::from_coverage(latest);
        for (seq, event) in (1_u64..).zip(&self.events) {
            if seq > latest {
                progress.observe(seq, event, Some(cut))?;
            }
        }
        if !progress.reached && cut != ProgressPoint::default() {
            return Err(SessionError::InvalidContextHistoryStart);
        }
        Ok(progress.coverage)
    }
}

impl Converted {
    pub(crate) fn source_bytes(&self) -> u64 {
        self.source_bytes
    }

    pub(crate) fn history_len(&self) -> usize {
        self.history_len
    }

    pub(crate) fn events(&self) -> &[ConversationEvent] {
        &self.events
    }

    #[must_use]
    pub(crate) fn rebound(mut self, id: &str) -> Self {
        id.clone_into(&mut self.metadata.id);
        self
    }

    pub(crate) fn summary(&self, title: Option<String>) -> SessionSummary {
        let metadata = &self.metadata;
        SessionSummary {
            id: metadata.id.clone(),
            workspace_root: metadata.workspace_root.clone(),
            origin_workspace_root: metadata.origin_workspace_root.clone(),
            title,
            created_at_ms: metadata.created_at_ms,
            updated_at_ms: metadata.updated_at_ms,
            conversation_language: metadata.conversation_language.clone(),
            history_len: self.history_len,
            has_checkpoint: false,
            source: SessionSource::OhFx,
        }
    }

    pub(crate) fn write(&self, copy: &PrivateDir) -> Result<(), SessionError> {
        store_new_results(
            copy,
            self.results
                .iter()
                .map(|result| (result.handle.as_str(), result.bytes.as_slice())),
        )?;
        let mut log = Vec::new();
        for (seq, event) in (1_u64..).zip(&self.events) {
            log.extend(encode_conversation_frame(
                seq,
                self.metadata.updated_at_ms,
                event,
            )?);
        }
        let mut events = create_managed_file(copy, EVENTS_FILE)?;
        events.write_all(&log)?;
        events.sync_all()?;
        if let Some(usage) = &self.usage {
            session_usage_sidecar::write(copy, &self.metadata.id, usage)?;
        }
        if let Some(recovery) = &self.recovery {
            copy.replace(RECOVERY_FILE, recovery)?;
        }
        copy.replace(MANIFEST_FILE, &encode_session_metadata(&self.metadata)?)?;
        sync_dir(copy)
    }
}

fn turn_events(
    turn: ConversationTurn,
    results: &mut Results,
) -> Result<Vec<ConversationEvent>, SessionError> {
    let ConversationTurn {
        user,
        work_id,
        execution,
        close,
    } = turn;
    let mut events = vec![ConversationEvent::User(match work_id {
        Some(work_id) => UserEvent::for_work(user, work_id),
        None => UserEvent::new(user),
    })];
    let Execution {
        steps,
        files,
        steering,
        turn_summary,
    } = execution;
    let ends_standalone = steps.last().is_some_and(|step| step.calls.is_empty());
    let mut steering = steering.into_iter().peekable();
    while let Some(entry) = steering.next_if(|entry| entry.after_tool_step_count == 0) {
        steering_events(entry, &mut events);
    }
    let mut follows_standalone = false;
    for (index, step) in steps.into_iter().enumerate() {
        let text = step.assistant.unwrap_or_default();
        if !text.is_empty() || step.replay.is_some() || follows_standalone {
            events.push(assistant(text, step.replay, step.calls.is_empty()));
        }
        follows_standalone = step.calls.is_empty();
        events.extend(step.calls.into_iter().map(ConversationEvent::ToolCall));
        for result in step.results {
            events.push(ConversationEvent::ToolResult(result_event(
                result, results,
            )?));
        }
        while let Some(entry) = steering.next_if(|entry| entry.after_tool_step_count == index + 1) {
            steering_events(entry, &mut events);
        }
    }
    if steering.next().is_some() {
        return Err(SessionError::InvalidConversationEvent);
    }
    let files: Vec<FileEvidence> = files
        .into_iter()
        .filter(|file| !file.path.is_empty())
        .collect();
    match close {
        TurnClose::Replied(reply, provider_replay) => {
            if !reply.is_empty() || provider_replay.is_some() || ends_standalone {
                events.push(assistant(reply, provider_replay, false));
            }
            events.push(ConversationEvent::TurnCompleted(TurnCompletedEvent {
                files,
                turn_summary,
            }));
        }
        TurnClose::Interrupted {
            reason,
            partial,
            pending,
            cancelled,
            ..
        } => {
            if let Some(call) = pending {
                if ends_standalone {
                    events.push(assistant(String::new(), None, false));
                }
                events.push(ConversationEvent::ToolCall(call));
            }
            let mut interrupted = InterruptedEvent::new(reason, partial);
            if let Some(cancelled) = cancelled {
                (
                    interrupted.command_replay_ref,
                    interrupted.command_replay_bytes,
                ) = CommandReplay::available(cancelled.replay.as_ref());
                interrupted.command_artifact_ref = cancelled.artifact;
            }
            interrupted.files = files;
            interrupted.turn_summary = turn_summary;
            events.push(ConversationEvent::Interrupted(interrupted));
        }
    }
    Ok(events)
}

fn assistant(
    text: String,
    provider_replay: Option<SavedReplay>,
    standalone_response: bool,
) -> ConversationEvent {
    ConversationEvent::Assistant(AssistantEvent {
        text,
        provider_replay,
        standalone_response,
    })
}

fn steering_events(entry: Steering, events: &mut Vec<ConversationEvent>) {
    if let Some(prefix) = entry.assistant_prefix.filter(|prefix| !prefix.is_empty()) {
        events.push(assistant(prefix, None, false));
    }
    if !entry.text.is_empty() {
        events.push(ConversationEvent::Steering(SteeringEvent {
            text: entry.text,
        }));
    }
}

fn result_event(
    result: SavedResult,
    results: &mut Results,
) -> Result<ToolResultEvent, SessionError> {
    let preview = match result.preview {
        Some(preview) => preview,
        None => bytes_preview(&result.output)
            .ok_or(SessionError::InvalidConversationEvent)?
            .to_owned(),
    };
    let (artifact_ref, stored_bytes, truncated) = if let Some(handle) = result.output_handle {
        (handle, result.stored_output_bytes, result.truncated)
    } else {
        let stored_bytes = u64::try_from(result.output.len()).unwrap_or(u64::MAX);
        let handle = results.output(&result.call_id, &result.tool_name, result.output);
        (handle, stored_bytes, true)
    };
    let completeness = if truncated {
        ArtifactCompleteness::Partial
    } else {
        ArtifactCompleteness::Complete
    };
    let presentation = result
        .presentation
        .map(|presentation| shown_presentation(&result.call_id, *presentation, results))
        .transpose()?;
    let (replay_ref, replay_bytes) = CommandReplay::available(result.replay.as_ref());
    let mut event = ToolResultEvent::new(
        result.call_id,
        result.tool_name,
        result.status,
        artifact_ref,
        stored_bytes,
        completeness,
    );
    event.output_bytes = Some(result.output_bytes);
    event.preview = Some(preview);
    event.provider_native = result.provider_native;
    event.created_at_ms = result.created_at_ms;
    event.permission_feedback = result.permission_feedback;
    event.command_process_presentation = result.process;
    event.committed_file_presentation = presentation;
    event.command_replay_ref = replay_ref;
    event.command_replay_bytes = replay_bytes;
    Ok(event)
}

fn shown_presentation(
    call_id: &str,
    legacy: LegacyPresentation,
    results: &mut Results,
) -> Result<Box<CommittedFilePresentation>, SessionError> {
    let LegacyPresentation {
        mut shown,
        previous_content,
        after_content,
    } = legacy;
    let size = |content: &Option<Vec<u8>>| content.as_ref().map_or(0, Vec::len);
    let inline = size(&previous_content).saturating_add(size(&after_content));
    let spills = shown.content_handle.is_none()
        && inline > PREVIEW_BYTES
        && fits_diff_pack(previous_content.as_deref(), after_content.as_deref());
    let packed = spills
        .then(|| {
            results.pack(
                call_id,
                previous_content.as_deref(),
                after_content.as_deref(),
            )
        })
        .flatten();
    if let Some(handle) = packed {
        shown.content_handle = Some(handle);
        return Ok(Box::new(shown));
    }
    let text = |content: Option<Vec<u8>>| {
        content
            .map(String::from_utf8)
            .transpose()
            .map_err(|_| SessionError::InvalidConversationEvent)
    };
    shown.previous_content = text(previous_content)?;
    shown.after_content = text(after_content)?;
    Ok(Box::new(shown))
}
