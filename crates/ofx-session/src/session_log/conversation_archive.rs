use std::mem;

use ofx_config::PrivateDir;
use ofx_contract::FileEvidence;

use super::conversation_writer::scan_log;
use super::managed_file::{Access, open_managed_file};
use super::{CompactedHistory, EVENTS_FILE, ReplayScan, read_checkpoint, read_metadata};
use crate::session_children::has_owner_marker;
use crate::session_error::SessionError;
use crate::session_event::{
    AssistantEvent, ConversationEvent, InterruptedEvent, SavedReplay, ToolCallEvent,
    ToolResultEvent,
};
use crate::session_migration::holds_schema_v3;
use crate::session_replay::History;
use crate::session_summary_codec::SessionSource;
use crate::session_usage_sidecar;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionArchive {
    pub id: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub conversation_language: String,
    pub turns: Vec<ArchivedTurn>,
    pub source: SessionSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchivedTurn {
    Compacted(CompactedHistory),
    Replied {
        user: String,
        assistant: String,
        execution: TurnExecution,
    },
    Interrupted {
        user: String,
        assistant: Option<String>,
        tool_call: Option<ToolCallEvent>,
        execution: TurnExecution,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnExecution {
    pub steps: Vec<ExecutedStep>,
    pub files: Vec<FileEvidence>,
    pub steering: Vec<ArchivedSteering>,
}

impl TurnExecution {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.files.is_empty() && self.steering.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutedStep {
    pub assistant: Option<String>,
    pub calls: Vec<ToolCallEvent>,
    pub results: Vec<ToolResultEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedSteering {
    pub text: String,
    pub assistant_prefix: Option<String>,
    pub after_tool_step_count: usize,
}

pub(crate) fn load_archive(
    dir: &PrivateDir,
    id: &str,
    unreadable_log: SessionError,
) -> Result<SessionArchive, SessionError> {
    if !matches!(has_owner_marker(dir), Ok(false)) {
        return Err(SessionError::SessionNotFound);
    }
    if holds_schema_v3(dir, id)? {
        return Err(SessionError::UnsupportedSessionSchema);
    }
    let metadata = read_metadata(dir, id)?;
    let archive = read_turns(dir).map_err(|_| unreadable_log)?;
    session_usage_sidecar::load_conversation(dir, id, metadata.updated_at_ms)?;
    read_checkpoint(dir, archive.checkpoint_seq())?;
    if metadata.subagent_child {
        return Err(SessionError::SessionNotFound);
    }
    Ok(SessionArchive {
        id: metadata.id,
        created_at_ms: metadata.created_at_ms,
        updated_at_ms: metadata.updated_at_ms,
        conversation_language: metadata.conversation_language,
        turns: archive.turns,
        source: SessionSource::OhFx,
    })
}

fn read_turns(dir: &PrivateDir) -> Result<ArchiveBuilder, SessionError> {
    let file = open_managed_file(dir, EVENTS_FILE, Access::ReadOnly)?
        .ok_or(SessionError::SessionNotFound)?;
    let history = History::log(&file);
    let scan = scan_log(
        history,
        file.metadata()?.len(),
        &mut ReplayScan::default(),
        None,
    )?;
    let mut builder = ArchiveBuilder::default();
    let mut frames = history.frames(0, scan.complete_bytes);
    while let Some(frame) = frames.next_frame()? {
        builder.apply(frame.envelope.seq, frame.envelope.event)?;
    }
    Ok(builder)
}

#[derive(Default)]
struct ArchiveBuilder {
    turns: Vec<ArchivedTurn>,
    raw_turn_count: usize,
    compaction_count: usize,
    last_seq: u64,
    open_turn_from: Option<u64>,
    user: Option<String>,
    pending_assistant: Option<String>,
    pending_replay: Option<SavedReplay>,
    calls: Vec<ToolCallEvent>,
    results: Vec<ToolResultEvent>,
    steps: Vec<ExecutedStep>,
    steering: Vec<ArchivedSteering>,
}

impl ArchiveBuilder {
    fn checkpoint_seq(&self) -> u64 {
        self.open_turn_from.unwrap_or(self.last_seq)
    }

    fn apply(&mut self, seq: u64, event: ConversationEvent) -> Result<(), SessionError> {
        self.last_seq = seq;
        match event {
            ConversationEvent::User(user) => {
                self.begin(user.text)?;
                self.open_turn_from = Some(seq.saturating_sub(1));
            }
            ConversationEvent::Assistant(assistant) => self.append_assistant(assistant)?,
            ConversationEvent::ToolCall(call) => self.append_tool_call(call)?,
            ConversationEvent::ToolResult(result) => self.append_tool_result(result)?,
            ConversationEvent::Steering(steering) => self.append_steering(steering.text)?,
            ConversationEvent::TurnCompleted(completed) => {
                let files = completed.files.into_iter().map(Into::into).collect();
                let turn = self.finish_assistant(files)?;
                self.complete(turn);
            }
            ConversationEvent::Interrupted(interrupted) => {
                let turn = self.finish_interrupted(interrupted)?;
                self.complete(turn);
            }
            ConversationEvent::ContextCheckpoint(checkpoint) => {
                if !self.calls.is_empty() || !self.results.is_empty() {
                    return Err(SessionError::InvalidConversationFrame);
                }
                self.finish_standalone()?;
                self.compaction_count += 1;
                if self.open_turn_from.is_some() {
                    self.open_turn_from = Some(seq);
                }
                self.turns.push(ArchivedTurn::Compacted(CompactedHistory {
                    summary: checkpoint.summary,
                    removed_turn_count: self.raw_turn_count,
                    compaction_count: self.compaction_count,
                }));
            }
        }
        Ok(())
    }

    fn complete(&mut self, turn: ArchivedTurn) {
        self.turns.push(turn);
        self.raw_turn_count += 1;
        self.open_turn_from = None;
    }

    fn is_idle(&self) -> bool {
        self.user.is_none()
            && self.pending_assistant.is_none()
            && self.calls.is_empty()
            && self.results.is_empty()
            && self.steps.is_empty()
            && self.steering.is_empty()
    }

    fn begin(&mut self, text: String) -> Result<(), SessionError> {
        if !self.is_idle() {
            return Err(SessionError::InvalidConversationFrame);
        }
        self.user = Some(text);
        Ok(())
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
            || self.calls.iter().any(|known| known.call_id == call.call_id)
        {
            return Err(SessionError::InvalidConversationFrame);
        }
        self.calls.push(call);
        Ok(())
    }

    fn append_tool_result(&mut self, result: ToolResultEvent) -> Result<(), SessionError> {
        let answers_a_call = self
            .calls
            .iter()
            .any(|call| call.call_id == result.call_id && call.tool_name == result.tool_name);
        let answered = self
            .results
            .iter()
            .any(|known| known.call_id == result.call_id);
        if self.user.is_none() || !answers_a_call || answered {
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
        self.steering.push(ArchivedSteering {
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
        self.pending_replay = None;
        self.steps.push(ExecutedStep {
            assistant: self.pending_assistant.take(),
            calls: mem::take(&mut self.calls),
            results: mem::take(&mut self.results),
        });
    }

    fn finish_assistant(&mut self, files: Vec<FileEvidence>) -> Result<ArchivedTurn, SessionError> {
        if !self.calls.is_empty() || !self.results.is_empty() {
            return Err(SessionError::InvalidConversationFrame);
        }
        let user = self
            .user
            .take()
            .ok_or(SessionError::InvalidConversationFrame)?;
        let assistant = self.pending_assistant.take().unwrap_or_default();
        self.pending_replay = None;
        Ok(ArchivedTurn::Replied {
            user,
            assistant,
            execution: self.take_execution(files),
        })
    }

    fn finish_interrupted(
        &mut self,
        interrupted: InterruptedEvent,
    ) -> Result<ArchivedTurn, SessionError> {
        if self.user.is_none() || !self.results.is_empty() || self.calls.len() > 1 {
            return Err(SessionError::InvalidConversationFrame);
        }
        if self.calls.is_empty() {
            self.finish_standalone()?;
        }
        let tool_call = self.calls.pop();
        self.pending_assistant = None;
        self.pending_replay = None;
        let user = self
            .user
            .take()
            .ok_or(SessionError::InvalidConversationFrame)?;
        let files = interrupted.files.into_iter().map(Into::into).collect();
        Ok(ArchivedTurn::Interrupted {
            user,
            assistant: interrupted.partial_text,
            tool_call,
            execution: self.take_execution(files),
        })
    }

    fn take_execution(&mut self, files: Vec<FileEvidence>) -> TurnExecution {
        TurnExecution {
            steps: mem::take(&mut self.steps),
            files,
            steering: mem::take(&mut self.steering),
        }
    }
}

#[cfg(test)]
mod tests;
