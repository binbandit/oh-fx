use std::io::Write as _;

use ofx_config::PrivateDir;

use super::LegacySession;
use super::durable_turn::{Execution, LegacyTurn, SavedResult, Steering, TurnClose};
use crate::result_store::{make_handle, preview, store_result};
use crate::session_codec::{SessionMetadata, encode_session_metadata};
use crate::session_display_metadata::history_title;
use crate::session_error::SessionError;
use crate::session_event::{
    ArtifactCompleteness, AssistantEvent, ConversationEvent, ConversationState, FileEvidence,
    InterruptedEvent, SteeringEvent, ToolResultEvent, TurnCompletedEvent, UserEvent,
    encode_conversation_frame,
};
use crate::session_log::managed_file::{create_managed_file, sync_dir};
use crate::session_log::{EVENTS_FILE, MANIFEST_FILE};
use crate::session_summary_codec::{SessionSource, SessionSummary};

pub(crate) struct Converted {
    metadata: SessionMetadata,
    turns: Vec<Vec<ConversationEvent>>,
    results: Vec<StoredResult>,
}

struct StoredResult {
    handle: String,
    text: String,
}

impl LegacySession {
    pub(super) fn convert(self) -> Result<Converted, SessionError> {
        let metadata = SessionMetadata {
            title: history_title(self.turns.iter().map(|turn| turn.user.as_str())),
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
        let turns = self
            .turns
            .into_iter()
            .map(|turn| turn_events(turn, &mut results))
            .collect::<Result<Vec<_>, _>>()?;
        let converted = Converted {
            metadata,
            turns,
            results,
        };
        converted.replay(|_, _| Ok(()))?;
        Ok(converted)
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
            history_len: self.turns.len(),
            has_checkpoint: false,
            source: SessionSource::OhFx,
        }
    }

    pub(crate) fn write(&self, copy: &PrivateDir) -> Result<(), SessionError> {
        for result in &self.results {
            store_result(copy, &result.handle, &result.text)?;
        }
        let mut log = Vec::new();
        self.replay(|seq, event| {
            log.extend(encode_conversation_frame(
                seq,
                self.metadata.updated_at_ms,
                event,
            )?);
            Ok(())
        })?;
        let mut events = create_managed_file(copy, EVENTS_FILE)?;
        events.write_all(&log)?;
        events.sync_all()?;
        copy.replace(MANIFEST_FILE, &encode_session_metadata(&self.metadata)?)?;
        sync_dir(copy)
    }

    fn replay(
        &self,
        mut visit: impl FnMut(u64, &ConversationEvent) -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        let mut state = ConversationState::default();
        for events in &self.turns {
            for event in events {
                let seq = state.next_seq()?;
                state.apply(seq, self.metadata.updated_at_ms, event)?;
                visit(seq, event)?;
            }
            if state.has_pending_tool_calls() {
                return Err(SessionError::UnresolvedToolCall);
            }
        }
        Ok(())
    }
}

fn turn_events(
    turn: LegacyTurn,
    results: &mut Vec<StoredResult>,
) -> Result<Vec<ConversationEvent>, SessionError> {
    let LegacyTurn {
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
            events.push(ConversationEvent::ToolResult(result_event(result, results)));
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
        } => {
            if let Some(call) = pending {
                if ends_standalone {
                    events.push(assistant(String::new(), false));
                }
                events.push(ConversationEvent::ToolCall(call));
            }
            let mut interrupted = InterruptedEvent::new(reason, partial);
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

fn result_event(result: SavedResult, results: &mut Vec<StoredResult>) -> ToolResultEvent {
    let preview = result
        .preview
        .unwrap_or_else(|| preview(&result.output).to_owned());
    let (artifact_ref, stored_bytes, truncated) = match result.output_handle {
        Some(handle) => (handle, result.stored_output_bytes, result.truncated),
        None => {
            let handle = make_handle(&result.call_id, &result.tool_name, &result.output);
            let stored_bytes = u64::try_from(result.output.len()).unwrap_or(u64::MAX);
            results.push(StoredResult {
                handle: handle.clone(),
                text: result.output,
            });
            (handle, stored_bytes, true)
        }
    };
    let completeness = if truncated {
        ArtifactCompleteness::Partial
    } else {
        ArtifactCompleteness::Complete
    };
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
    event
}
