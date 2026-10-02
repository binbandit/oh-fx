use ofx_contract::HistoryCut;

use crate::session_error::SessionError;
use crate::session_event::ConversationEvent;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProgressPoint {
    pub(crate) turns: usize,
    pub(crate) tool_steps: usize,
    pub(crate) steering: usize,
}

impl From<HistoryCut> for ProgressPoint {
    fn from(cut: HistoryCut) -> Self {
        Self {
            turns: cut.turns,
            tool_steps: cut.tool_steps,
            steering: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingAssistant {
    seq: u64,
    has_replay: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ConversationProgress {
    pub(crate) point: ProgressPoint,
    pub(crate) coverage: u64,
    pub(crate) reached: bool,
    pending: usize,
    pending_assistant: Option<PendingAssistant>,
}

impl ConversationProgress {
    pub(crate) fn from_coverage(coverage: u64) -> Self {
        Self {
            coverage,
            ..Self::default()
        }
    }

    pub(crate) fn observe(
        &mut self,
        seq: u64,
        event: &ConversationEvent,
        cut: Option<ProgressPoint>,
    ) -> Result<(), SessionError> {
        if let Some(assistant) = self.pending_assistant.take() {
            let standalone = match event {
                ConversationEvent::Assistant(_)
                | ConversationEvent::ContextCheckpoint(_)
                | ConversationEvent::Interrupted(_) => true,
                ConversationEvent::Steering(_) => assistant.has_replay,
                _ => false,
            };
            if standalone {
                self.point.tool_steps += 1;
                if !self.reached && cut == Some(self.point) {
                    self.coverage = assistant.seq;
                    self.reached = true;
                }
            }
        }
        if cut == Some(self.point) && self.pending == 0 {
            self.reached = true;
        }
        match event {
            ConversationEvent::Assistant(assistant) => {
                if assistant.standalone_response {
                    self.point.tool_steps += 1;
                } else if !assistant.text.is_empty() || assistant.provider_replay.is_some() {
                    self.pending_assistant = Some(PendingAssistant {
                        seq,
                        has_replay: assistant.provider_replay.is_some(),
                    });
                }
            }
            ConversationEvent::ToolCall(_) => self.pending += 1,
            ConversationEvent::ToolResult(_) => {
                let pending = self
                    .pending
                    .checked_sub(1)
                    .ok_or(SessionError::InvalidConversationFrame)?;
                self.pending = pending;
                if pending == 0 {
                    self.point.tool_steps += 1;
                }
            }
            ConversationEvent::Steering(_) => self.point.steering += 1,
            ConversationEvent::TurnCompleted(_) | ConversationEvent::Interrupted(_) => {
                self.point = ProgressPoint {
                    turns: self.point.turns + 1,
                    ..ProgressPoint::default()
                };
                self.pending = 0;
            }
            ConversationEvent::User(_) | ConversationEvent::ContextCheckpoint(_) => {}
        }
        if !self.reached && cut == Some(self.point) && self.pending == 0 {
            self.coverage = seq;
            self.reached = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_event::{AssistantEvent, UserEvent};

    fn assistant(text: &str) -> ConversationEvent {
        ConversationEvent::Assistant(AssistantEvent {
            text: text.to_owned(),
            provider_replay: None,
            standalone_response: false,
        })
    }

    #[test]
    fn a_cut_counts_standalone_text_as_a_completed_step() {
        let cut = Some(ProgressPoint {
            tool_steps: 1,
            ..ProgressPoint::default()
        });
        let mut progress = ConversationProgress::default();
        let user = ConversationEvent::User(UserEvent::new("request"));
        progress.observe(1, &user, cut).unwrap();
        progress
            .observe(2, &assistant("CANDIDATE_741"), cut)
            .unwrap();
        progress.observe(3, &assistant("FINAL_852"), cut).unwrap();
        assert_eq!(progress.point.tool_steps, 1);
        assert_eq!(progress.coverage, 2);
        assert!(progress.reached);
    }
}
