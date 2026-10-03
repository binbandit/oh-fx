use ofx_contract::{ToolArgumentIntegrity, ToolResultStatus};

use super::{
    AssistantEvent, CONVERSATION_SCHEMA_VERSION, ContextCheckpointEvent, ConversationEnvelope,
    ConversationEvent, FileEvidence, FileEvidenceAction, InterruptedEvent, SavedReplay,
    SavedReplaySource, SteeringEvent, ToolCallEvent, ToolResultEvent, TurnCompletedEvent,
    UserEvent, WireTag,
};
use crate::json_fields::{Fields, Json, string};
use crate::session_codec::parse_saved_provider;

pub(super) fn envelope_from(document: Json<'_>) -> Option<ConversationEnvelope> {
    let mut fields = Fields::new(document)?;
    let schema_version = fields.or("schema_version", CONVERSATION_SCHEMA_VERSION, |value| {
        u8::try_from(value.as_u64()?).ok()
    })?;
    let envelope = ConversationEnvelope {
        schema_version,
        seq: fields.unsigned("seq")?,
        timestamp_ms: fields.signed("timestamp_ms")?,
        event: event_from(fields.required("event")?)?,
    };
    fields.finish(envelope)
}

fn event_from(value: Json<'_>) -> Option<ConversationEvent> {
    let Json::Object(tagged) = value else {
        return None;
    };
    if tagged.len() != 1 {
        return None;
    }
    let (tag, body) = tagged.into_iter().next()?;
    let mut fields = Fields::new(body)?;
    let event = match tag.as_ref() {
        "user" => ConversationEvent::User(user(&mut fields)?),
        "assistant" => ConversationEvent::Assistant(assistant(&mut fields)?),
        "tool_call" => ConversationEvent::ToolCall(tool_call(&mut fields)?),
        "tool_result" => ConversationEvent::ToolResult(tool_result(&mut fields)?),
        "steering" => ConversationEvent::Steering(SteeringEvent {
            text: fields.string("text")?,
        }),
        "turn_completed" => ConversationEvent::TurnCompleted(TurnCompletedEvent {
            files: fields.or("files", Vec::new(), files)?,
            turn_summary: fields.fixed("turn_summary")?,
        }),
        "interrupted" => ConversationEvent::Interrupted(interrupted(&mut fields)?),
        "context_checkpoint" => ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
            covers_through_seq: fields.unsigned("covers_through_seq")?,
            summary: fields.string("summary")?,
        }),
        _ => return None,
    };
    fields.finish(event)
}

fn user(fields: &mut Fields<'_>) -> Option<UserEvent> {
    Some(UserEvent {
        text: fields.string("text")?,
        images: fields.fixed("images")?,
        work_id: fields.fixed("work_id")?,
    })
}

fn assistant(fields: &mut Fields<'_>) -> Option<AssistantEvent> {
    Some(AssistantEvent {
        text: fields.string("text")?,
        provider_replay: fields
            .nullable("provider_replay", |value| saved_replay(value).map(Some))?,
        standalone_response: fields.or("standalone_response", false, |value| value.as_bool())?,
    })
}

pub(crate) fn saved_replay(value: Json<'_>) -> Option<SavedReplay> {
    let mut fields = Fields::new(value)?;
    let replay = SavedReplay {
        source: replay_source(fields.required("source")?)?,
        parts_json: fields.string("parts_json")?,
    };
    fields.finish(replay)
}

fn replay_source(value: Json<'_>) -> Option<SavedReplaySource> {
    let mut fields = Fields::new(value)?;
    let source = SavedReplaySource {
        provider: parse_saved_provider(&fields.required("provider")?)?,
        model: fields.string("model")?,
    };
    fields.finish(source)
}

fn tool_call(fields: &mut Fields<'_>) -> Option<ToolCallEvent> {
    Some(ToolCallEvent {
        call_id: fields.string("call_id")?,
        tool_name: fields.string("tool_name")?,
        arguments_json: fields.string("arguments_json")?,
        argument_integrity: fields.or(
            "argument_integrity",
            ToolArgumentIntegrity::Valid,
            |value| tag(&value),
        )?,
        provisional_id: fields.fixed("provisional_id")?,
        provider_result: fields.fixed("provider_result")?,
        final_identity: fields.fixed("final_identity")?,
        provenance: fields.fixed("provenance")?,
    })
}

fn tool_result(fields: &mut Fields<'_>) -> Option<ToolResultEvent> {
    Some(ToolResultEvent {
        call_id: fields.string("call_id")?,
        tool_name: fields.string("tool_name")?,
        status: tag(&fields.required("status")?)?,
        artifact_ref: fields.string("artifact_ref")?,
        tool_image_handle: fields.fixed("tool_image_handle")?,
        output_bytes: fields.nullable("output_bytes", |value| value.as_u64().map(Some))?,
        stored_bytes: fields.unsigned("stored_bytes")?,
        completeness: tag(&fields.required("completeness")?)?,
        preview: fields.nullable("preview", |value| string(value).map(Some))?,
        provider_native: fields.fixed("provider_native")?,
        review_feedback: fields.fixed("review_feedback")?,
        created_at_ms: fields.or("created_at_ms", 0, |value| value.as_i64())?,
        permission_feedback: fields.fixed("permission_feedback")?,
        committed_file_presentation: fields.fixed("committed_file_presentation")?,
        command_replay_ref: fields.fixed("command_replay_ref")?,
        command_replay_bytes: fields.fixed("command_replay_bytes")?,
        command_process_presentation: fields.fixed("command_process_presentation")?,
        terminal_action_presentation: fields.fixed("terminal_action_presentation")?,
    })
}

fn interrupted(fields: &mut Fields<'_>) -> Option<InterruptedEvent> {
    Some(InterruptedEvent {
        reason: tag(&fields.required("reason")?)?,
        partial_text: fields.nullable("partial_text", |value| string(value).map(Some))?,
        command_replay_ref: fields.fixed("command_replay_ref")?,
        command_replay_bytes: fields.fixed("command_replay_bytes")?,
        command_artifact_ref: fields.fixed("command_artifact_ref")?,
        files: fields.or("files", Vec::new(), files)?,
        turn_summary: fields.fixed("turn_summary")?,
        cancellation_origin: fields.fixed("cancellation_origin")?,
    })
}

fn files(value: Json<'_>) -> Option<Vec<FileEvidence>> {
    let Json::List(items) = value else {
        return None;
    };
    items.into_iter().map(file_evidence).collect()
}

fn file_evidence(value: Json<'_>) -> Option<FileEvidence> {
    let mut fields = Fields::new(value)?;
    let file = FileEvidence {
        path: fields.string("path")?,
        new_path: fields.nullable("new_path", |value| string(value).map(Some))?,
        tool_call_id: fields.string("tool_call_id")?,
        tool_name: fields.string("tool_name")?,
        action: fields.or("action", FileEvidenceAction::Unknown, |value| tag(&value))?,
        status: fields.or("status", ToolResultStatus::Success, |value| tag(&value))?,
        model_view_covers_full_file: fields.or("model_view_covers_full_file", false, |value| {
            value.as_bool()
        })?,
        stale: fields.or("stale", false, |value| value.as_bool())?,
    };
    fields.finish(file)
}

fn tag<T: WireTag>(value: &Json<'_>) -> Option<T> {
    T::from_tag(value.as_str()?)
}

#[cfg(test)]
mod tests;
