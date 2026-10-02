use ofx_config::{PrivateDir, ProviderId};
use ofx_contract::{
    HistoryStep, HistoryTurn, ProviderReplay, ToolArgumentIntegrity, TurnEnd, TurnStop,
};

use crate::result_store::{make_handle, preview, store_result};
use crate::session_codec::SavedProvider;
use crate::session_error::SessionError;
use crate::session_event::{
    ArtifactCompleteness, AssistantEvent, ConversationEvent, InterruptReason, InterruptedEvent,
    SavedReplay, SavedReplaySource, ToolCallEvent, ToolResultEvent, TurnCompletedEvent, UserEvent,
};

pub(crate) struct TurnArtifacts<'a> {
    pub(crate) dir: &'a PrivateDir,
    pub(crate) provider: &'a SavedProvider,
    pub(crate) timestamp_ms: i64,
}

pub(crate) fn turn_events(
    artifacts: &TurnArtifacts<'_>,
    turn: &HistoryTurn<'_>,
    written_steps: usize,
) -> Result<Vec<ConversationEvent>, SessionError> {
    let steps = turn
        .steps
        .get(written_steps..)
        .ok_or(SessionError::InvalidContextHistoryStart)?;
    let mut events = vec![ConversationEvent::User(UserEvent::new(turn.user))];
    for (index, step) in steps.iter().enumerate() {
        let follows_standalone = index > 0 && steps[index - 1].tool_calls.is_empty();
        step_events(artifacts, step, follows_standalone, &mut events)?;
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
            events.push(ConversationEvent::TurnCompleted(
                TurnCompletedEvent::default(),
            ));
        }
        TurnEnd::Stopped { reason, partial } => {
            let reason = match reason {
                TurnStop::Cancelled => InterruptReason::Cancelled,
                TurnStop::Failed => InterruptReason::Failed,
            };
            let partial = (!partial.is_empty()).then(|| partial.to_owned());
            events.push(ConversationEvent::Interrupted(InterruptedEvent::new(
                reason, partial,
            )));
        }
    }
    Ok(events)
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
        events.push(ConversationEvent::ToolCall(ToolCallEvent::new(
            call.id.as_str(),
            call.name.as_str(),
            call.arguments.as_str(),
            ToolArgumentIntegrity::classify_function_input(&call.arguments),
        )));
    }
    for result in &step.tool_results {
        let handle = make_handle(result.call_id, result.tool_name, result.output);
        store_result(artifacts.dir, &handle, result.output)?;
        let bytes = u64::try_from(result.output.len())
            .map_err(|_| SessionError::InvalidConversationEvent)?;
        let mut event = ToolResultEvent::new(
            result.call_id,
            result.tool_name,
            result.status,
            handle,
            bytes,
            ArtifactCompleteness::Complete,
        );
        event.output_bytes = Some(bytes);
        event.preview = Some(preview(result.output).to_owned());
        event.created_at_ms = artifacts.timestamp_ms;
        events.push(ConversationEvent::ToolResult(event));
    }
    Ok(())
}

fn saved_replay(replay: &ProviderReplay, running: &SavedProvider) -> Option<SavedReplay> {
    let id = ProviderId::parse(&replay.source.provider)?;
    let provider = if id == *running.id() {
        running.clone()
    } else {
        SavedProvider::new(id, None)?
    };
    Some(SavedReplay {
        source: SavedReplaySource {
            provider,
            model: replay.source.model.clone(),
        },
        parts_json: replay.parts_json.clone(),
    })
}
