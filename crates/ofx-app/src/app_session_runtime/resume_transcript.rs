use std::collections::HashMap;

use ofx_contract::{
    CallDescription, HistoryEntry, Notice, NoticeTone, SavedToolCall, ToolCallId, ToolResultStatus,
};
use ofx_session::{
    ConversationEvent, InterruptReason, SavedTurn, SessionError, ToolResultEvent, WritableSession,
};
use ofx_tools::answered_questions;

const RESUMED_TOPIC: &str = "session resumed";
const SYSTEM_TOPIC: &str = "system";
const FAILED_TURN: &str = "failed";

pub(crate) type DescribeSaved<'a> = &'a dyn Fn(&str, &str) -> Option<CallDescription>;

pub(super) fn transcript(
    session: &WritableSession,
    title: &str,
    describe: DescribeSaved<'_>,
) -> Result<Vec<HistoryEntry>, SessionError> {
    let mut entries = vec![HistoryEntry::Notice(Notice::new(
        NoticeTone::Neutral,
        RESUMED_TOPIC,
        title,
    ))];
    session.visit_transcript(|turn| {
        let replay = TurnReplay {
            session,
            describe,
            shown: Vec::new(),
            running: HashMap::new(),
        };
        entries.extend(replay.replay(turn));
    })?;
    Ok(entries)
}

struct RunningCall {
    slot: usize,
    tool_name: String,
    arguments: String,
    description: Option<CallDescription>,
}

struct TurnReplay<'a, 'b> {
    session: &'a WritableSession,
    describe: DescribeSaved<'b>,
    shown: Vec<Option<HistoryEntry>>,
    running: HashMap<String, RunningCall>,
}

impl TurnReplay<'_, '_> {
    fn replay(mut self, turn: SavedTurn) -> impl Iterator<Item = HistoryEntry> {
        for event in turn.events {
            match event {
                ConversationEvent::ToolCall(call) => {
                    let description = (self.describe)(&call.tool_name, &call.arguments_json);
                    let running = RunningCall {
                        slot: self.shown.len(),
                        tool_name: call.tool_name,
                        arguments: call.arguments_json,
                        description,
                    };
                    self.running.insert(call.call_id, running);
                    self.shown.push(None);
                }
                ConversationEvent::ToolResult(result) => self.finish(result),
                ConversationEvent::User(user) => self.show(HistoryEntry::User(user.text)),
                ConversationEvent::Steering(steering) if !steering.text.is_empty() => {
                    self.show(HistoryEntry::User(steering.text));
                }
                ConversationEvent::Assistant(assistant) if !assistant.text.is_empty() => {
                    self.show(HistoryEntry::Assistant(assistant.text));
                }
                ConversationEvent::Interrupted(interrupted) => {
                    if let Some(partial) = interrupted.partial_text.filter(|text| !text.is_empty())
                    {
                        self.show(HistoryEntry::Assistant(partial));
                    }
                    self.show(match interrupted.reason {
                        InterruptReason::Cancelled => HistoryEntry::Cancelled,
                        InterruptReason::Failed => HistoryEntry::Notice(Notice::new(
                            NoticeTone::Error,
                            SYSTEM_TOPIC,
                            FAILED_TURN,
                        )),
                    });
                }
                _ => {}
            }
        }
        self.shown.into_iter().flatten()
    }

    fn show(&mut self, entry: HistoryEntry) {
        self.shown.push(Some(entry));
    }

    fn finish(&mut self, result: ToolResultEvent) {
        let Some(call) = self.running.remove(&result.call_id) else {
            return;
        };
        let output = self
            .session
            .tool_result_output(&result)
            .or(result.preview)
            .unwrap_or_default();
        let answers = (result.status == ToolResultStatus::Success)
            .then(|| answered_questions(&result.tool_name, || Some(output.clone())))
            .flatten();
        let entry = match answers {
            Some(answers) => HistoryEntry::QuestionsAnswered(answers),
            None => HistoryEntry::Tool(SavedToolCall {
                call_id: ToolCallId::new(result.call_id),
                tool_name: call.tool_name,
                arguments: call.arguments,
                description: call.description,
                status: result.status,
                output,
            }),
        };
        self.shown[call.slot] = Some(entry);
    }
}
