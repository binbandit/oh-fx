use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, ToolResultEvent};
use crate::session_replay::{FrameReader, History};

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

impl SavedTurn {
    pub(crate) fn reply_for_work(&self, work_id: &str) -> Option<String> {
        let ConversationEvent::User(user) = self.events.first()? else {
            return None;
        };
        if user.work_id.as_deref() != Some(work_id) {
            return None;
        }
        let mut reply = String::new();
        for event in &self.events {
            match event {
                ConversationEvent::Assistant(assistant) => reply.clone_from(&assistant.text),
                ConversationEvent::Interrupted(interrupted) => {
                    return Some(interrupted.partial_text.clone().unwrap_or_default());
                }
                _ => {}
            }
        }
        Some(reply)
    }
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
        history: History<'_>,
        start: u64,
        end: u64,
    ) -> Result<(), SessionError> {
        let mut frames = history.frames(start, end);
        while let Some(frame) = frames.next_frame()? {
            self.observe(frame.offset, frame.envelope.seq, &frame.envelope.event)?;
        }
        Ok(())
    }

    pub(crate) fn rewind(&mut self, last_seq: u64, turn_open: bool) {
        self.last_seq = last_seq;
        if !turn_open {
            self.active_user_offset = None;
        }
    }

    pub(crate) fn finish(
        &self,
        history: History<'_>,
        end: u64,
    ) -> Result<ReplayWindow, SessionError> {
        let mut window = self.window;
        if window.checkpoint_offset.is_none() {
            return Ok(window);
        }
        window.offset = 0;
        window.active_user_offset = None;
        window.prior_turn_count = 0;
        let mut frames = history.frames(0, end);
        while let Some(frame) = frames.next_frame()? {
            if frame.envelope.seq > window.coverage {
                break;
            }
            match frame.envelope.event {
                ConversationEvent::User(_) => window.active_user_offset = Some(frame.offset),
                ConversationEvent::TurnCompleted(_) | ConversationEvent::Interrupted(_) => {
                    window.active_user_offset = None;
                    window.prior_turn_count += 1;
                }
                _ => {}
            }
            window.offset = frames.offset();
        }
        Ok(window)
    }
}

pub(crate) fn replay_history(
    history: History<'_>,
    end: u64,
    window: &ReplayWindow,
) -> Result<SavedHistory, SessionError> {
    let compacted = match window.checkpoint_offset {
        Some(offset) => match history.event_at(offset, end)? {
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
        Some(offset) => match history.event_at(offset, end)? {
            user @ ConversationEvent::User(_) => Some(vec![user]),
            _ => return Err(SessionError::InvalidConversationFrame),
        },
        None => None,
    };
    let mut turns = Vec::new();
    group_turns(history.frames(window.offset, end), current, |turn| {
        turns.push(turn);
    })?;
    Ok(SavedHistory { compacted, turns })
}

pub(crate) fn visit_turns(
    history: History<'_>,
    end: u64,
    visit: impl FnMut(SavedTurn),
) -> Result<(), SessionError> {
    group_turns(history.frames(0, end), None, visit)
}

fn group_turns(
    mut frames: FrameReader<'_>,
    mut current: Option<Vec<ConversationEvent>>,
    mut visit: impl FnMut(SavedTurn),
) -> Result<(), SessionError> {
    while let Some(frame) = frames.next_frame()? {
        let event = frame.envelope.event;
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
