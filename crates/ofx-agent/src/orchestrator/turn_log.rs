use ofx_contract::{
    ChatMessage, ConversationLog, HistoryCut, HistoryTurn, LogFailure, RestoredHistory, TurnEnd,
    TurnStop,
};

use super::Agent;
use crate::compactor::{Compacted, encode_checkpoint, restore_checkpoint};
use crate::execution_memory::{history_turn, logged_steps};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ending {
    Replied,
    Stopped(TurnStop),
    Discarded,
}

impl Agent {
    #[must_use]
    pub fn with_conversation_log(mut self, log: Box<dyn ConversationLog>) -> Self {
        self.log = Some(log);
        self
    }

    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn restore(&mut self, restored: RestoredHistory) {
        let RestoredHistory {
            checkpoint,
            mut messages,
            mut turn_starts,
        } = restored;
        self.compacted = None;
        if let Some(summary) = checkpoint {
            let (text, payload) = restore_checkpoint(&summary);
            messages.insert(0, ChatMessage::user(text));
            for start in &mut turn_starts {
                *start += 1;
            }
            self.compacted = payload;
        }
        self.history = messages;
        self.turn_starts = turn_starts;
        self.calibration = None;
    }

    pub(super) fn record_turn(
        &mut self,
        prompt: &str,
        start: usize,
        ending: Ending,
    ) -> Result<(), LogFailure> {
        let Some(log) = self.log.as_mut() else {
            return Ok(());
        };
        let parsed = (ending != Ending::Discarded && start < self.history.len())
            .then(|| history_turn(&self.history, start, self.history.len()));
        let turn = match (parsed, ending) {
            (Some(parsed), Ending::Replied) => HistoryTurn {
                user: parsed.user,
                steps: logged_steps(&parsed.steps),
                end: TurnEnd::Replied {
                    text: parsed.reply,
                    provider_replay: parsed.reply_replay,
                },
            },
            (Some(parsed), Ending::Stopped(reason)) => HistoryTurn {
                user: parsed.user,
                steps: logged_steps(&parsed.steps),
                end: TurnEnd::Stopped {
                    reason,
                    partial: parsed.reply,
                },
            },
            _ => HistoryTurn {
                user: prompt,
                steps: Vec::new(),
                end: TurnEnd::Stopped {
                    reason: TurnStop::Failed,
                    partial: "",
                },
            },
        };
        log.record_turn(&turn)
    }

    pub(super) fn record_compaction(
        &mut self,
        start: usize,
        compacted: &Compacted,
    ) -> Result<(), LogFailure> {
        let Some(log) = self.log.as_mut() else {
            return Ok(());
        };
        let parsed = history_turn(&self.history, start, self.history.len());
        let active = HistoryTurn {
            user: parsed.user,
            steps: logged_steps(&parsed.steps),
            end: TurnEnd::Replied {
                text: "",
                provider_replay: None,
            },
        };
        let cut = HistoryCut {
            turns: compacted.cut.turns,
            tool_steps: compacted.cut.tool_steps,
        };
        log.record_compaction(&encode_checkpoint(&compacted.payload), cut, &active)
    }
}
