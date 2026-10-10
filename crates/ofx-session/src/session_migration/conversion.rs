use std::io::Write as _;

use ofx_config::PrivateDir;

use super::LegacySession;
use super::durable_turn::{
    ConversationTurn, Execution, LegacyTurn, SavedResult, Steering, TurnClose,
};
use super::legacy_presentation::CommandReplay;
use super::recovery_file::recovery_file;
use crate::result_store::{
    PREVIEW_BYTES, bytes_handle, bytes_preview, diff_content_pack, store_new_results,
};
use crate::session_codec::{SessionMetadata, encode_session_metadata};
use crate::session_display_metadata::history_title;
use crate::session_error::SessionError;
use crate::session_event::{
    ArtifactCompleteness, AssistantEvent, CommittedFilePresentation, ContextCheckpointEvent,
    ConversationEvent, ConversationState, FileEvidence, InterruptedEvent, SteeringEvent,
    ToolResultEvent, TurnCompletedEvent, UserEvent, encode_conversation_frame,
};
use crate::session_log::managed_file::{create_managed_file, sync_dir};
use crate::session_log::{
    ConversationProgress, EVENTS_FILE, MANIFEST_FILE, ProgressPoint, RECOVERY_FILE,
};
use crate::session_summary_codec::{SessionSource, SessionSummary};

pub(crate) struct Converted {
    metadata: SessionMetadata,
    events: Vec<ConversationEvent>,
    history_len: usize,
    results: Vec<StoredResult>,
    recovery: Option<Vec<u8>>,
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

impl LegacySession {
    pub(super) fn convert(self) -> Result<Converted, SessionError> {
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
        let mut results = Vec::new();
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
            results.extend(file.spilled.into_iter().map(|(handle, text)| StoredResult {
                handle,
                bytes: text.into_bytes(),
            }));
            file.bytes
        });
        Ok(Converted {
            metadata,
            events: log.events,
            history_len,
            results,
            recovery,
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
        if let Some(recovery) = &self.recovery {
            copy.replace(RECOVERY_FILE, recovery)?;
        }
        copy.replace(MANIFEST_FILE, &encode_session_metadata(&self.metadata)?)?;
        sync_dir(copy)
    }
}

fn turn_events(
    turn: ConversationTurn,
    results: &mut Vec<StoredResult>,
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
        if !step.assistant.is_empty() || follows_standalone {
            events.push(assistant(step.assistant, step.calls.is_empty()));
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
        TurnClose::Replied(reply) => {
            if !reply.is_empty() || ends_standalone {
                events.push(assistant(reply, false));
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
        } => {
            if let Some(call) = pending {
                if ends_standalone {
                    events.push(assistant(String::new(), false));
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

fn assistant(text: String, standalone_response: bool) -> ConversationEvent {
    ConversationEvent::Assistant(AssistantEvent {
        text,
        provider_replay: None,
        standalone_response,
    })
}

fn steering_events(entry: Steering, events: &mut Vec<ConversationEvent>) {
    if !entry.assistant_prefix.is_empty() {
        events.push(assistant(entry.assistant_prefix, false));
    }
    if !entry.text.is_empty() {
        events.push(ConversationEvent::Steering(SteeringEvent {
            text: entry.text,
        }));
    }
}

fn result_event(
    result: SavedResult,
    results: &mut Vec<StoredResult>,
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
        let handle = bytes_handle(&result.call_id, &result.tool_name, &result.output);
        let stored_bytes = u64::try_from(result.output.len()).unwrap_or(u64::MAX);
        results.push(StoredResult {
            handle: handle.clone(),
            bytes: result.output,
        });
        (handle, stored_bytes, true)
    };
    let completeness = if truncated {
        ArtifactCompleteness::Partial
    } else {
        ArtifactCompleteness::Complete
    };
    let presentation = result
        .presentation
        .map(|presentation| spill_contents(&result.call_id, presentation, results));
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

fn spill_contents(
    call_id: &str,
    mut presentation: Box<CommittedFilePresentation>,
    results: &mut Vec<StoredResult>,
) -> Box<CommittedFilePresentation> {
    let content_bytes = |content: &Option<String>| content.as_ref().map_or(0, String::len);
    let inline = content_bytes(&presentation.previous_content)
        .saturating_add(content_bytes(&presentation.after_content));
    if presentation.content_handle.is_some() || inline <= PREVIEW_BYTES {
        return presentation;
    }
    let Some((handle, pack)) = diff_content_pack(
        call_id,
        presentation.previous_content.as_deref(),
        presentation.after_content.as_deref(),
    ) else {
        return presentation;
    };
    presentation.previous_content = None;
    presentation.after_content = None;
    presentation.content_handle = Some(handle.clone());
    results.push(StoredResult {
        handle,
        bytes: pack,
    });
    presentation
}
