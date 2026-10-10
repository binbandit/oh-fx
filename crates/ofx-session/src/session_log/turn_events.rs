use ofx_config::{PrivateDir, ProviderId};
use ofx_contract::{
    HistorySteering, HistoryStep, HistoryTurn, ProviderReplay, ToolArgumentIntegrity, TurnEnd,
    TurnStop,
};

use crate::result_store::{make_handle, preview, store_result};
use crate::session_codec::SavedProvider;
use crate::session_error::SessionError;
use crate::session_event::{
    ArtifactCompleteness, AssistantEvent, CancellationOrigin, ConversationEvent, FileEvidence,
    InterruptReason, InterruptedEvent, SavedReplay, SavedReplaySource, SteeringEvent,
    ToolCallEvent, ToolResultEvent, TurnCompletedEvent, UserEvent,
};
use crate::session_log::conversation_progress::ProgressPoint;

pub(crate) struct TurnArtifacts<'a> {
    pub(crate) dir: &'a PrivateDir,
    pub(crate) provider: &'a SavedProvider,
    pub(crate) timestamp_ms: i64,
    pub(crate) work_id: Option<&'a str>,
}

pub(crate) fn turn_events(
    artifacts: &TurnArtifacts<'_>,
    turn: &HistoryTurn<'_>,
    written: ProgressPoint,
) -> Result<Vec<ConversationEvent>, SessionError> {
    let steps = turn
        .steps
        .get(written.tool_steps..)
        .ok_or(SessionError::InvalidContextHistoryStart)?;
    let steering = turn
        .steering
        .get(written.steering..)
        .ok_or(SessionError::InvalidContextHistoryStart)?;
    if steering
        .iter()
        .any(|entry| entry.after_tool_step_count > turn.steps.len())
    {
        return Err(SessionError::InvalidConversationEvent);
    }
    let mut steering = steering.iter().peekable();
    let user = match artifacts.work_id {
        Some(work_id) => UserEvent::for_work(turn.user, work_id),
        None => UserEvent::new(turn.user),
    };
    let mut events = vec![ConversationEvent::User(user)];
    for (index, step) in steps.iter().enumerate() {
        let position = written.tool_steps + index;
        while let Some(entry) = steering.next_if(|entry| entry.after_tool_step_count <= position) {
            steering_events(entry, &mut events);
        }
        let follows_standalone = index > 0 && steps[index - 1].tool_calls.is_empty();
        step_events(artifacts, step, follows_standalone, &mut events)?;
    }
    for entry in steering {
        steering_events(entry, &mut events);
    }
    let follows_standalone = steps.last().is_some_and(|step| step.tool_calls.is_empty());
    match turn.end {
        TurnEnd::Replied {
            text,
            provider_replay,
        } => {
            if !text.is_empty() || provider_replay.is_some() || follows_standalone {
                events.push(ConversationEvent::Assistant(AssistantEvent {
                    text: text.to_owned(),
                    provider_replay: provider_replay
                        .and_then(|replay| saved_replay(replay, artifacts.provider)),
                    standalone_response: false,
                }));
            }
            events.push(ConversationEvent::TurnCompleted(TurnCompletedEvent {
                files: turn.files.iter().map(FileEvidence::from).collect(),
                ..TurnCompletedEvent::default()
            }));
        }
        TurnEnd::Stopped { reason, partial } => {
            let (reason, origin) = match reason {
                TurnStop::Cancelled => (InterruptReason::Cancelled, CancellationOrigin::Turn),
                TurnStop::CompactionCancelled => {
                    (InterruptReason::Cancelled, CancellationOrigin::Compaction)
                }
                TurnStop::Failed => (InterruptReason::Failed, CancellationOrigin::Turn),
            };
            let partial = (!partial.is_empty()).then(|| partial.to_owned());
            let mut interrupted = InterruptedEvent::new(reason, partial);
            interrupted.cancellation_origin = origin;
            interrupted.files = turn.files.iter().map(FileEvidence::from).collect();
            events.push(ConversationEvent::Interrupted(interrupted));
        }
    }
    Ok(events)
}

fn steering_events(steering: &HistorySteering<'_>, events: &mut Vec<ConversationEvent>) {
    if !steering.assistant_prefix.is_empty() {
        events.push(ConversationEvent::Assistant(AssistantEvent {
            text: steering.assistant_prefix.to_owned(),
            provider_replay: None,
            standalone_response: false,
        }));
    }
    if !steering.text.is_empty() {
        events.push(ConversationEvent::Steering(SteeringEvent {
            text: steering.text.to_owned(),
        }));
    }
}

fn step_events(
    artifacts: &TurnArtifacts<'_>,
    step: &HistoryStep<'_>,
    follows_standalone: bool,
    events: &mut Vec<ConversationEvent>,
) -> Result<(), SessionError> {
    if !step.assistant.is_empty() || step.provider_replay.is_some() || follows_standalone {
        events.push(ConversationEvent::Assistant(AssistantEvent {
            text: step.assistant.to_owned(),
            provider_replay: step
                .provider_replay
                .and_then(|replay| saved_replay(replay, artifacts.provider)),
            standalone_response: step.tool_calls.is_empty(),
        }));
    }
    for call in step.tool_calls {
        let mut event = ToolCallEvent::new(
            call.id.as_str(),
            call.name.as_str(),
            call.arguments.as_str(),
            ToolArgumentIntegrity::classify_function_input(&call.arguments),
        );
        event.provider_result.clone_from(&call.provider_result);
        event.provenance = call.provenance;
        events.push(ConversationEvent::ToolCall(event));
    }
    for result in &step.tool_results {
        let handle = make_handle(result.call_id, result.tool_name, result.output);
        store_result(artifacts.dir, &handle, result.output)?;
        let stored_bytes = u64::try_from(result.output.len())
            .map_err(|_| SessionError::InvalidConversationEvent)?;
        let output_bytes = u64::try_from(result.output_bytes)
            .map_err(|_| SessionError::InvalidConversationEvent)?;
        let mut event = ToolResultEvent::new(
            result.call_id,
            result.tool_name,
            result.status,
            handle,
            stored_bytes,
            ArtifactCompleteness::Complete,
        );
        event.output_bytes = Some(output_bytes);
        event.preview = Some(preview(result.output).to_owned());
        event.created_at_ms = artifacts.timestamp_ms;
        event.command_process_presentation = result.process;
        event.review_feedback = result.review_feedback;
        event.permission_feedback = result
            .permission_feedback
            .iter()
            .map(|feedback| (*feedback).to_owned())
            .collect();
        events.push(ConversationEvent::ToolResult(event));
    }
    Ok(())
}

pub(crate) fn saved_replay(
    replay: &ProviderReplay,
    running: &SavedProvider,
) -> Option<SavedReplay> {
    let id = ProviderId::parse(&replay.source.provider)?;
    let provider = match replay.source.binding {
        Some(binding) => SavedProvider::new(id, Some(binding))?,
        None if id == *running.id() => running.clone(),
        None => SavedProvider::new(id, None)?,
    };
    Some(SavedReplay {
        source: SavedReplaySource {
            provider,
            model: replay.source.model.clone(),
        },
        parts_json: replay.parts_json.clone(),
    })
}
