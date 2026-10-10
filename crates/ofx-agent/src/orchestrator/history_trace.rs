use std::ops::Range;

use ofx_contract::{ChatMessage, INTERRUPTED_BEFORE_COMPLETION, INTERRUPTED_TURN_CONTEXT};
use ofx_trace::{TraceContext, trace_event, trace_log};

const HISTORY: &str = "history";
const NONE: &str = "none";
const CLOSING_BREAK: &str = "\n\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnKind {
    CompactedSummary,
    Assistant,
    Interrupted { partial_text: bool },
}

impl TurnKind {
    const fn name(self) -> &'static str {
        match self {
            Self::CompactedSummary => "compacted_summary",
            Self::Assistant => "assistant",
            Self::Interrupted { .. } => "interrupted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HistoryShape {
    turns: usize,
    interrupted: usize,
    partial_closures: usize,
    kinds: String,
    roles: String,
    messages: usize,
}

pub(super) fn shape(
    history: &[ChatMessage],
    starts: &[usize],
    open: &[Range<usize>],
) -> Option<HistoryShape> {
    ofx_trace::enabled(HISTORY).then(|| HistoryShape::of(history, starts, open))
}

pub(super) fn projected(context: TraceContext, shape: Option<HistoryShape>, before: usize) {
    let Some(shape) = shape else {
        return;
    };
    let context = TraceContext {
        step_id: 0,
        ..context
    };
    let HistoryShape {
        turns,
        interrupted,
        partial_closures,
        kinds,
        roles,
        messages,
    } = &shape;
    trace_event!(
        HISTORY,
        "projection_start",
        context,
        "history_turns={turns} gateway_messages_before={before} interrupted_turns={interrupted} history_turn_kinds={kinds}"
    );
    trace_event!(
        HISTORY,
        "projection_end",
        context,
        "history_turns={turns} gateway_messages={} added_gateway_messages={messages} interrupted_turns={interrupted} history_turn_kinds={kinds} projected_message_roles={roles} partial_interrupted_closures={partial_closures}",
        before + messages
    );
}

pub(super) fn tool_history_projected(messages: usize) {
    trace_log!(
        HISTORY,
        "legacy_tool_history_projected terminal=true subagent=false messages={messages}"
    );
}

impl HistoryShape {
    fn of(history: &[ChatMessage], starts: &[usize], open: &[Range<usize>]) -> Self {
        let summarized = starts
            .first()
            .map_or(!history.is_empty(), |first| *first > 0);
        let turns = starts.iter().enumerate().map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(history.len());
            let still_open = open.iter().any(|range| range.start == *start);
            turn_kind(history.get(*start..end).unwrap_or_default(), still_open)
        });
        let kinds: Vec<TurnKind> = summarized
            .then_some(TurnKind::CompactedSummary)
            .into_iter()
            .chain(turns)
            .collect();
        let interrupted = kinds
            .iter()
            .filter(|kind| matches!(kind, TurnKind::Interrupted { .. }))
            .count();
        let partial_closures = kinds
            .iter()
            .filter(|kind| **kind == TurnKind::Interrupted { partial_text: true })
            .count();
        Self {
            turns: kinds.len(),
            interrupted,
            partial_closures,
            kinds: joined(kinds.iter().map(|kind| kind.name())),
            roles: joined(history.iter().map(role)),
            messages: history.len(),
        }
    }
}

fn turn_kind(messages: &[ChatMessage], still_open: bool) -> TurnKind {
    let closed = matches!(
        messages.last(),
        Some(ChatMessage::User { content, feedback_for: None, .. }) if content == INTERRUPTED_TURN_CONTEXT
    );
    if !closed && !still_open {
        return TurnKind::Assistant;
    }
    let used_tools = messages.iter().any(|message| match message {
        ChatMessage::Tool { .. } => true,
        ChatMessage::Assistant { tool_calls, .. } => !tool_calls.is_empty(),
        ChatMessage::System { .. } | ChatMessage::User { .. } => false,
    });
    let partial_text = !used_tools
        && messages.iter().any(|message| {
            matches!(message, ChatMessage::Assistant { content: Some(text), .. } if !partial(text).is_empty())
        });
    TurnKind::Interrupted { partial_text }
}

fn partial(text: &str) -> &str {
    text.strip_suffix(INTERRUPTED_BEFORE_COMPLETION)
        .map_or(text, |rest| {
            rest.strip_suffix(CLOSING_BREAK).unwrap_or(rest)
        })
}

const fn role(message: &ChatMessage) -> &'static str {
    match message {
        ChatMessage::System { .. } => "system",
        ChatMessage::User { .. } => "user",
        ChatMessage::Assistant { .. } => "assistant",
        ChatMessage::Tool { .. } => "tool",
    }
}

fn joined<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let joined = names.collect::<Vec<_>>().join(",");
    if joined.is_empty() {
        NONE.to_owned()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests;
