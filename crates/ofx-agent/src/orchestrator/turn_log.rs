use ofx_contract::{
    ChatMessage, ConversationLog, HistoryTurn, LogFailure, ModelRecoveryCause, RecoveryPoint,
    RecoveryProgress, RestoredHistory, TurnEnd, TurnStop,
};

use super::turn_ledger::TurnRecord;
use super::{Agent, Turn};
use crate::compactor::{Compacted, encode_checkpoint, restore_checkpoint};
use crate::execution_memory::{history_turn, logged_steps};
use crate::model_response_recovery::DEFAULT_MAX_PROVIDER_ATTEMPTS;

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
        self.ledger.reset(turn_starts.len());
        self.turn_starts = turn_starts;
        self.calibration = None;
    }

    pub(super) fn require_writable(&self) -> Result<(), LogFailure> {
        self.log
            .as_ref()
            .map_or(Ok(()), |log| log.require_writable())
    }

    pub(super) fn note_recorded(&mut self, turn: &Turn, ending: Ending, saved: bool) {
        let record = match ending {
            Ending::Discarded if saved && turn.compaction.checkpointed() => TurnRecord::LogOnly,
            Ending::Discarded => return,
            _ if saved => TurnRecord::Saved,
            _ => TurnRecord::Unsaved,
        };
        self.ledger.push(record);
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
                steering: parsed.logged_steering(),
                end: TurnEnd::Replied {
                    text: parsed.reply,
                    provider_replay: parsed.reply_replay,
                },
            },
            (Some(parsed), Ending::Stopped(reason)) => HistoryTurn {
                user: parsed.user,
                steps: logged_steps(&parsed.steps, &turn.raw_outputs),
                steering: parsed.logged_steering(),
                end: TurnEnd::Stopped {
                    reason,
                    partial: parsed.reply,
                },
            },
            _ => HistoryTurn {
                user: prompt,
                steps: Vec::new(),
                steering: Vec::new(),
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
        let active = turn.map(|turn| turn_so_far(&self.history, turn));
        let cut = self.ledger.logged_cut(compacted.cut);
        log.record_compaction(&encode_checkpoint(&compacted.payload), cut, active.as_ref())
    }

    pub(super) fn record_recovery(
        &self,
        turn: &Turn,
        cause: ModelRecoveryCause,
        progress: RecoveryProgress,
        consumed_attempts: usize,
    ) -> Result<(), LogFailure> {
        let Some(log) = self.log.as_ref() else {
            return Ok(());
        };
        if turn.start >= self.history.len() {
            return Ok(());
        }
        log.record_recovery(&RecoveryPoint {
            turn_id: turn.id,
            turn: turn_so_far(&self.history, turn),
            cause,
            progress,
            model: &self.config.model,
            requested_fast_mode: self.config.fast_mode,
            fast_mode: turn.fast_mode,
            attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
            consumed_attempts,
        })
    }

    pub(super) fn discard_recovery(&self) {
        if let Some(log) = self.log.as_ref() {
            let _ = log.clear_recovery();
        }
    }
}

fn turn_so_far<'a>(history: &'a [ChatMessage], turn: &Turn) -> HistoryTurn<'a> {
    let parsed = history_turn(history, turn.start, history.len());
    HistoryTurn {
        user: parsed.user,
        steps: logged_steps(&parsed.steps, &turn.raw_outputs),
        steering: parsed.logged_steering(),
        end: TurnEnd::Replied {
            text: "",
            provider_replay: None,
        },
    }
}
