use std::fs::File;

use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, ToolResultEvent, decode_conversation_frame};
use crate::session_replay::{LineRead, LineReader};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedHistory {
    pub compacted: Option<CompactedHistory>,
    pub turns: Vec<SavedTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactedHistory {
    pub summary: String,
    pub removed_turn_count: usize,
    pub compaction_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedTurn {
    pub events: Vec<ConversationEvent>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReplayWindow {
    offset: u64,
    prior_turn_count: usize,
    compaction_count: usize,
    active_user_offset: Option<u64>,
    checkpoint_offset: Option<u64>,
    coverage: u64,
}

#[derive(Debug, Default)]
pub(crate) struct ReplayScan {
    window: ReplayWindow,
    turn_count: usize,
    last_seq: u64,
    active_user_offset: Option<u64>,
}

impl ReplayScan {
    pub(crate) fn observe(
        &mut self,
        offset: u64,
        seq: u64,
        event: &ConversationEvent,
    ) -> Result<(), SessionError> {
        if self.last_seq.checked_add(1) != Some(seq) {
            return Err(SessionError::InvalidConversationFrame);
        }
        self.last_seq = seq;
        match event {
            ConversationEvent::User(_) => {
                if self.active_user_offset.replace(offset).is_some() {
                    return Err(SessionError::InvalidConversationFrame);
                }
            }
            ConversationEvent::TurnCompleted(_) | ConversationEvent::Interrupted(_) => {
                if self.active_user_offset.take().is_none() {
                    return Err(SessionError::InvalidConversationFrame);
                }
                self.turn_count = self
                    .turn_count
                    .checked_add(1)
                    .ok_or(SessionError::InvalidConversationFrame)?;
            }
            ConversationEvent::ContextCheckpoint(checkpoint) => {
                self.window = ReplayWindow {
                    offset,
                    prior_turn_count: self.turn_count,
                    compaction_count: self
                        .window
                        .compaction_count
                        .checked_add(1)
                        .ok_or(SessionError::InvalidConversationFrame)?,
                    active_user_offset: self.active_user_offset,
                    checkpoint_offset: Some(offset),
                    coverage: checkpoint.covers_through_seq,
                };
            }
            ConversationEvent::Assistant(_)
            | ConversationEvent::ToolCall(_)
            | ConversationEvent::ToolResult(_)
            | ConversationEvent::Steering(_) => {}
        }
        Ok(())
    }

    pub(crate) fn observe_range(
        &mut self,
        file: &File,
        start: u64,
        end: u64,
    ) -> Result<(), SessionError> {
        let mut reader = LineReader::new(file, start, end)?;
        loop {
            let offset = reader.offset();
            let LineRead::Line(line) = reader.next_line()? else {
                return Ok(());
            };
            let envelope = decode_conversation_frame(&line)?;
            self.observe(offset, envelope.seq, &envelope.event)?;
        }
    }

    pub(crate) fn rewind(&mut self, last_seq: u64, turn_open: bool) {
        self.last_seq = last_seq;
        if !turn_open {
            self.active_user_offset = None;
        }
    }

    pub(crate) fn finish(&self, file: &File, end: u64) -> Result<ReplayWindow, SessionError> {
        let mut window = self.window;
        if window.checkpoint_offset.is_none() {
            return Ok(window);
        }
        window.offset = 0;
        window.active_user_offset = None;
        window.prior_turn_count = 0;
        let mut reader = LineReader::new(file, 0, end)?;
        loop {
            let offset = reader.offset();
            let LineRead::Line(line) = reader.next_line()? else {
                break;
            };
            let envelope = decode_conversation_frame(&line)?;
            if envelope.seq > window.coverage {
                break;
            }
            match envelope.event {
                ConversationEvent::User(_) => window.active_user_offset = Some(offset),
                ConversationEvent::TurnCompleted(_) | ConversationEvent::Interrupted(_) => {
                    window.active_user_offset = None;
                    window.prior_turn_count += 1;
                }
                _ => {}
            }
            window.offset = reader.offset();
        }
        Ok(window)
    }
}

pub(crate) fn replay_history(
    file: &File,
    end: u64,
    window: &ReplayWindow,
) -> Result<SavedHistory, SessionError> {
    let compacted = match window.checkpoint_offset {
        Some(offset) => match read_event_at(file, offset, end)? {
            ConversationEvent::ContextCheckpoint(checkpoint) => Some(CompactedHistory {
                summary: checkpoint.summary,
                removed_turn_count: window.prior_turn_count,
                compaction_count: window.compaction_count,
            }),
            _ => return Err(SessionError::InvalidConversationFrame),
        },
        None => None,
    };
    let current = match window.active_user_offset {
        Some(offset) => match read_event_at(file, offset, end)? {
            user @ ConversationEvent::User(_) => Some(vec![user]),
            _ => return Err(SessionError::InvalidConversationFrame),
        },
        None => None,
    };
    let mut turns = Vec::new();
    let reader = LineReader::new(file, window.offset, end)?;
    group_turns(reader, current, |turn| turns.push(turn))?;
    Ok(SavedHistory { compacted, turns })
}

pub(crate) fn visit_turns(
    file: &File,
    end: u64,
    visit: impl FnMut(SavedTurn),
) -> Result<(), SessionError> {
    group_turns(LineReader::new(file, 0, end)?, None, visit)
}

fn group_turns(
    mut reader: LineReader<'_>,
    mut current: Option<Vec<ConversationEvent>>,
    mut visit: impl FnMut(SavedTurn),
) -> Result<(), SessionError> {
    while let LineRead::Line(line) = reader.next_line()? {
        let event = decode_conversation_frame(&line)?.event;
        match event {
            ConversationEvent::User(_) => {
                if current.replace(vec![event]).is_some() {
                    return Err(SessionError::InvalidConversationFrame);
                }
            }
            ConversationEvent::TurnCompleted(_) | ConversationEvent::Interrupted(_) => {
                let mut events = current
                    .take()
                    .ok_or(SessionError::InvalidConversationFrame)?;
                events.push(event);
                visit(SavedTurn { events });
            }
            ConversationEvent::ContextCheckpoint(_) => {
                if let Some(events) = current.as_mut() {
                    events.push(event);
                }
            }
            ConversationEvent::ToolResult(ref result) => {
                let events = current
                    .as_mut()
                    .filter(|events| answers_a_replayed_call(events, result))
                    .ok_or(SessionError::InvalidConversationFrame)?;
                events.push(event);
            }
            ConversationEvent::Assistant(_)
            | ConversationEvent::ToolCall(_)
            | ConversationEvent::Steering(_) => current
                .as_mut()
                .ok_or(SessionError::InvalidConversationFrame)?
                .push(event),
        }
    }
    Ok(())
}

fn answers_a_replayed_call(events: &[ConversationEvent], result: &ToolResultEvent) -> bool {
    events.iter().any(|event| {
        matches!(event, ConversationEvent::ToolCall(call)
            if call.call_id == result.call_id && call.tool_name == result.tool_name)
    })
}

fn read_event_at(file: &File, offset: u64, end: u64) -> Result<ConversationEvent, SessionError> {
    match LineReader::new(file, offset, end)?.next_line()? {
        LineRead::Line(line) => Ok(decode_conversation_frame(&line)?.event),
        LineRead::End | LineRead::Torn => Err(SessionError::InvalidConversationFrame),
    }
}
