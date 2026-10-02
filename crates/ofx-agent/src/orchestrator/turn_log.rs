use ofx_contract::{
    ChatMessage, ConversationLog, HistoryCut, HistoryTurn, LogFailure, RestoredHistory, TurnEnd,
    TurnStop,
};

use super::{Agent, Turn};
use crate::compactor::{Compacted, encode_checkpoint, restore_checkpoint};
use crate::execution_memory::{history_turn, logged_steps};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ending {
    Replied,
    Stopped(TurnStop),
    Discarded,
}

impl Agent {
    pub fn attach_session(&mut self, session_id: String, log: Box<dyn ConversationLog>) {
        self.session_id = Some(session_id);
        self.log = Some(log);
    }

    pub fn detach_session(&mut self) {
        self.session_id = None;
        self.log = None;
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

    pub(super) fn require_writable(&self) -> Result<(), LogFailure> {
        self.log
            .as_ref()
            .map_or(Ok(()), |log| log.require_writable())
    }

    pub(super) fn record_turn(
        &mut self,
        prompt: &str,
        turn: &Turn,
        ending: Ending,
    ) -> Result<(), LogFailure> {
        let start = turn.start;
        let Some(log) = self.log.as_mut() else {
            return Ok(());
        };
        let parsed = (ending != Ending::Discarded && start < self.history.len())
            .then(|| history_turn(&self.history, start, self.history.len()));
        let logged = match (parsed, ending) {
            (Some(parsed), Ending::Replied) => HistoryTurn {
                user: parsed.user,
                steps: logged_steps(&parsed.steps, &turn.raw_outputs),
                end: TurnEnd::Replied {
                    text: parsed.reply,
                    provider_replay: parsed.reply_replay,
                },
            },
            (Some(parsed), Ending::Stopped(reason)) => HistoryTurn {
                user: parsed.user,
                steps: logged_steps(&parsed.steps, &turn.raw_outputs),
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
        log.record_turn(&logged)
    }

    pub(super) fn record_compaction(
        &mut self,
        turn: Option<&Turn>,
        compacted: &Compacted,
    ) -> Result<(), LogFailure> {
        let Some(log) = self.log.as_mut() else {
            return Ok(());
        };
        let active = turn.map(|turn| {
            let parsed = history_turn(&self.history, turn.start, self.history.len());
            HistoryTurn {
                user: parsed.user,
                steps: logged_steps(&parsed.steps, &turn.raw_outputs),
                end: TurnEnd::Replied {
                    text: "",
                    provider_replay: None,
                },
            }
        });
        let cut = HistoryCut {
            turns: compacted.cut.turns,
            tool_steps: compacted.cut.tool_steps,
        };
        log.record_compaction(&encode_checkpoint(&compacted.payload), cut, active.as_ref())
    }
}
