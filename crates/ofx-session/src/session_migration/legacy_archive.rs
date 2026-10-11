use super::LegacySession;
use super::durable_turn::{
    CompactedSummary, ConversationTurn, Execution, LegacyTurn, SavedResult, Steering, Step,
    TurnClose,
};
use super::legacy_presentation::LegacyPresentation;
use crate::session_json::presentation_json;
use crate::session_log::{
    ArchivedResult, ArchivedSteering, ArchivedTurn, CompactedHistory, ExecutedStep, SessionArchive,
    TurnExecution,
};
use crate::session_summary_codec::SessionSource;

impl LegacySession {
    pub(super) fn archive(self) -> SessionArchive {
        SessionArchive {
            id: self.id,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            conversation_language: self.conversation_language,
            turns: self.turns.into_iter().map(archived_turn).collect(),
            source: SessionSource::OhFx,
        }
    }
}

fn archived_turn(turn: LegacyTurn) -> ArchivedTurn {
    match turn {
        LegacyTurn::Compacted(CompactedSummary {
            summary,
            removed_turn_count,
            compaction_count,
        }) => ArchivedTurn::Compacted(CompactedHistory {
            summary,
            removed_turn_count,
            compaction_count,
        }),
        LegacyTurn::Conversation(turn) => conversation_turn(*turn),
    }
}

fn conversation_turn(turn: ConversationTurn) -> ArchivedTurn {
    let execution = turn_execution(turn.execution);
    match turn.close {
        TurnClose::Replied(assistant, _) => ArchivedTurn::Replied {
            user: turn.user,
            assistant,
            execution,
        },
        TurnClose::Interrupted {
            partial,
            pending,
            completed,
            ..
        } => ArchivedTurn::Interrupted {
            user: turn.user,
            assistant: partial,
            tool_call: pending,
            completed_tool_names: completed,
            execution,
        },
    }
}

fn turn_execution(execution: Execution) -> TurnExecution {
    TurnExecution {
        steps: execution.steps.into_iter().map(executed_step).collect(),
        files: execution.files.into_iter().map(Into::into).collect(),
        steering: execution.steering.into_iter().map(steering).collect(),
    }
}

fn executed_step(step: Step) -> ExecutedStep {
    ExecutedStep {
        assistant: step.assistant,
        calls: step.calls,
        results: step.results.into_iter().map(archived_result).collect(),
    }
}

fn archived_result(result: SavedResult) -> ArchivedResult {
    ArchivedResult {
        call_id: result.call_id,
        tool_name: result.tool_name,
        status: result.status,
        output: lossy(result.output),
        output_handle: result.output_handle,
        preview: result.preview,
        output_bytes: result.output_bytes,
        stored_output_bytes: result.stored_output_bytes,
        truncated: result.truncated,
        provider_native: result.provider_native,
        created_at_ms: result.created_at_ms,
        permission_feedback: result.permission_feedback,
        presentation: result
            .presentation
            .map(|presentation| shown_presentation(*presentation)),
    }
}

fn shown_presentation(presentation: LegacyPresentation) -> serde_json::Value {
    let mut shown = presentation.shown;
    shown.previous_content = presentation.previous_content.map(lossy);
    shown.after_content = presentation.after_content.map(lossy);
    presentation_json(&shown)
}

fn steering(entry: Steering) -> ArchivedSteering {
    ArchivedSteering {
        text: entry.text,
        assistant_prefix: entry.assistant_prefix,
        after_tool_step_count: entry.after_tool_step_count,
    }
}

fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}
