use std::mem;

use ofx_config::PrivateDir;
use ofx_contract::{ChatMessage, RestoredHistory, ToolCall, ToolCallId, ToolResultStatus};

use crate::result_store::{RESULT_UNAVAILABLE, format_stored_result_output, read_for_replay};
use crate::session_error::SessionError;
use crate::session_event::{
    ArtifactCompleteness, AssistantEvent, ConversationEvent, SavedReplay, ToolCallEvent,
    ToolResultEvent,
};
use crate::session_log::conversation_history::SavedHistory;

const INTERRUPTED_BEFORE_COMPLETION: &str = "The previous response ended before completion.";
const ABORTED_TOOL_OUTPUT: &str = "aborted by user";
const INTERRUPTED_TURN_CONTEXT: &str = "<turn_aborted>\nThe previous turn ended before completion. Any tools or commands may have partially executed. Do not continue this request unless the user explicitly asks to continue.\n</turn_aborted>";

struct Step {
    assistant: Option<String>,
    replay: Option<SavedReplay>,
    calls: Vec<ToolCallEvent>,
    results: Vec<ToolResultEvent>,
}

struct Steering {
    text: String,
    assistant_prefix: Option<String>,
    after_tool_step_count: usize,
}

enum Ending {
    Replied {
        text: String,
        replay: Option<SavedReplay>,
    },
    Stopped {
        partial: Option<String>,
        pending_call: Option<ToolCallEvent>,
    },
}

#[derive(Default)]
struct TurnBuilder {
    user: Option<String>,
    pending_assistant: Option<String>,
    pending_replay: Option<SavedReplay>,
    calls: Vec<ToolCallEvent>,
    results: Vec<ToolResultEvent>,
    steps: Vec<Step>,
    steering: Vec<Steering>,
}

impl TurnBuilder {
    fn observe(&mut self, event: ConversationEvent) -> Result<Option<Ending>, SessionError> {
        match event {
            ConversationEvent::User(user) => {
                if self.user.replace(user.text).is_some() {
                    return Err(SessionError::InvalidConversationFrame);
                }
            }
            ConversationEvent::Assistant(assistant) => self.append_assistant(assistant)?,
            ConversationEvent::ToolCall(call) => self.append_tool_call(call)?,
            ConversationEvent::ToolResult(result) => self.append_tool_result(result)?,
            ConversationEvent::Steering(steering) => self.append_steering(steering.text)?,
            ConversationEvent::ContextCheckpoint(_) => self.finish_standalone()?,
            ConversationEvent::TurnCompleted(_) => {
                if !self.calls.is_empty() || !self.results.is_empty() {
                    return Err(SessionError::InvalidConversationFrame);
                }
                return Ok(Some(Ending::Replied {
                    text: self.pending_assistant.take().unwrap_or_default(),
                    replay: self.pending_replay.take(),
                }));
            }
            ConversationEvent::Interrupted(interrupted) => {
                if !self.results.is_empty() || self.calls.len() > 1 {
                    return Err(SessionError::InvalidConversationFrame);
                }
                if self.calls.is_empty() {
                    self.finish_standalone()?;
                }
                self.pending_assistant = None;
                self.pending_replay = None;
                return Ok(Some(Ending::Stopped {
                    partial: interrupted.partial_text,
                    pending_call: self.calls.pop(),
                }));
            }
        }
        Ok(None)
    }

    fn append_assistant(&mut self, assistant: AssistantEvent) -> Result<(), SessionError> {
        if self.calls.is_empty() && self.results.is_empty() {
            self.finish_standalone()?;
        }
        if self.user.is_none() || self.pending_assistant.is_some() || !self.calls.is_empty() {
            return Err(SessionError::InvalidConversationFrame);
        }
        self.pending_assistant = Some(assistant.text);
        self.pending_replay = assistant.provider_replay;
        if assistant.standalone_response {
            self.finish_step();
        }
        Ok(())
    }

    fn append_tool_call(&mut self, call: ToolCallEvent) -> Result<(), SessionError> {
        if self.user.is_none()
            || !self.results.is_empty()
            || self.calls.iter().any(|seen| seen.call_id == call.call_id)
        {
            return Err(SessionError::InvalidConversationFrame);
        }
        self.calls.push(call);
        Ok(())
    }

    fn append_tool_result(&mut self, result: ToolResultEvent) -> Result<(), SessionError> {
        let matching = self
            .calls
            .iter()
            .find(|call| call.call_id == result.call_id)
            .is_some_and(|call| call.tool_name == result.tool_name);
        let repeated = self
            .results
            .iter()
            .any(|seen| seen.call_id == result.call_id);
        if self.user.is_none() || !matching || repeated {
            return Err(SessionError::InvalidConversationFrame);
        }
        self.results.push(result);
        if self.results.len() == self.calls.len() {
            self.finish_step();
        }
        Ok(())
    }

    fn append_steering(&mut self, text: String) -> Result<(), SessionError> {
        if self.user.is_none() || !self.calls.is_empty() || !self.results.is_empty() {
            return Err(SessionError::InvalidConversationFrame);
        }
        if self.pending_replay.is_some() {
            self.finish_step();
        }
        self.steering.push(Steering {
            text,
            assistant_prefix: self.pending_assistant.take(),
            after_tool_step_count: self.steps.len(),
        });
        Ok(())
    }

    fn finish_standalone(&mut self) -> Result<(), SessionError> {
        if !self.calls.is_empty() || !self.results.is_empty() {
            return Err(SessionError::InvalidConversationFrame);
        }
        match &self.pending_assistant {
            None => {}
            Some(text) if text.is_empty() && self.pending_replay.is_none() => {
                self.pending_assistant = None;
            }
            Some(_) => self.finish_step(),
        }
        Ok(())
    }

    fn finish_step(&mut self) {
        self.steps.push(Step {
            assistant: self.pending_assistant.take(),
            replay: self.pending_replay.take(),
            calls: mem::take(&mut self.calls),
            results: mem::take(&mut self.results),
        });
    }
}

pub(crate) fn restored_history(
    history: SavedHistory,
    dir: &PrivateDir,
) -> Result<RestoredHistory, SessionError> {
    let mut messages = Vec::new();
    let mut turn_starts = Vec::with_capacity(history.turns.len());
    for turn in history.turns {
        let mut builder = TurnBuilder::default();
        let mut ending = None;
        for event in turn.events {
            if ending.is_some() {
                return Err(SessionError::InvalidConversationFrame);
            }
            ending = builder.observe(event)?;
        }
        let (Some(user), Some(ending)) = (builder.user.take(), ending) else {
            return Err(SessionError::InvalidConversationFrame);
        };
        turn_starts.push(messages.len());
        messages.push(ChatMessage::user(user));
        push_execution(&mut messages, builder.steps, builder.steering, dir);
        push_ending(&mut messages, ending);
    }
    Ok(RestoredHistory {
        checkpoint: history.compacted.map(|compacted| compacted.summary),
        messages,
        turn_starts,
    })
}

fn push_execution(
    messages: &mut Vec<ChatMessage>,
    steps: Vec<Step>,
    steering: Vec<Steering>,
    dir: &PrivateDir,
) {
    let mut steering = steering.into_iter().peekable();
    for (index, step) in steps.into_iter().enumerate() {
        while let Some(entry) = steering.next_if(|entry| entry.after_tool_step_count == index) {
            push_steering(messages, entry);
        }
        if step.calls.is_empty() && step.replay.is_none() && step.assistant.is_none() {
            continue;
        }
        messages.push(ChatMessage::Assistant {
            content: step.assistant,
            tool_calls: step.calls.into_iter().map(tool_call).collect(),
            provider_replay: step.replay.map(SavedReplay::into_provider_replay),
        });
        for result in step.results {
            let content = result_body(&result, dir);
            messages.push(ChatMessage::Tool {
                call_id: ToolCallId::new(result.call_id),
                tool_name: result.tool_name,
                content,
                status: result.status,
            });
        }
    }
    for entry in steering {
        push_steering(messages, entry);
    }
}

fn push_steering(messages: &mut Vec<ChatMessage>, entry: Steering) {
    if let Some(prefix) = entry.assistant_prefix.filter(|prefix| !prefix.is_empty()) {
        messages.push(ChatMessage::Assistant {
            content: Some(prefix),
            tool_calls: Vec::new(),
            provider_replay: None,
        });
    }
    messages.push(ChatMessage::restored_steering(entry.text));
}

fn push_ending(messages: &mut Vec<ChatMessage>, ending: Ending) {
    match ending {
        Ending::Replied { text, replay } => {
            if !text.is_empty() || replay.is_some() {
                messages.push(ChatMessage::Assistant {
                    content: Some(text),
                    tool_calls: Vec::new(),
                    provider_replay: replay.map(SavedReplay::into_provider_replay),
                });
            }
        }
        Ending::Stopped {
            partial,
            pending_call,
        } => {
            let partial = partial.filter(|text| !text.is_empty());
            match pending_call {
                Some(call) => {
                    let call = tool_call(call);
                    messages.push(ChatMessage::Assistant {
                        content: partial,
                        tool_calls: vec![call.clone()],
                        provider_replay: None,
                    });
                    messages.push(ChatMessage::Tool {
                        call_id: call.id,
                        tool_name: call.name,
                        content: ABORTED_TOOL_OUTPUT.to_owned(),
                        status: ToolResultStatus::Failure,
                    });
                }
                None => messages.push(ChatMessage::Assistant {
                    content: Some(match partial {
                        Some(text) => format!("{text}\n\n{INTERRUPTED_BEFORE_COMPLETION}"),
                        None => INTERRUPTED_BEFORE_COMPLETION.to_owned(),
                    }),
                    tool_calls: Vec::new(),
                    provider_replay: None,
                }),
            }
            messages.push(ChatMessage::user(INTERRUPTED_TURN_CONTEXT));
        }
    }
}

fn result_body(result: &ToolResultEvent, dir: &PrivateDir) -> String {
    if result.completeness != ArtifactCompleteness::Complete {
        let preview = result.preview.as_deref().unwrap_or_default();
        return format_stored_result_output(&result.artifact_ref, preview, result.stored_bytes);
    }
    complete_result_output(result, dir).unwrap_or_else(|| RESULT_UNAVAILABLE.to_owned())
}

pub(crate) fn complete_result_output(result: &ToolResultEvent, dir: &PrivateDir) -> Option<String> {
    if result.completeness != ArtifactCompleteness::Complete {
        return None;
    }
    let preview = result.preview.as_deref().unwrap_or_default();
    if u64::try_from(preview.len()).is_ok_and(|length| length == result.stored_bytes) {
        return Some(preview.to_owned());
    }
    read_for_replay(dir, &result.artifact_ref, result.stored_bytes)
}

fn tool_call(call: ToolCallEvent) -> ToolCall {
    ToolCall::new(call.call_id, call.tool_name, call.arguments_json)
}

#[cfg(test)]
mod tests;
