use ofx_contract::{
    ChatMessage, ConversationLog, FileEvidence, HistoryTurn, LogFailure, ModelRecoveryAction,
    ModelRecoveryCause, RecoveryPoint, RecoveryProgress, RestoredHistory, TurnEnd, TurnStop,
    file_evidence_context,
};

use super::recovery::Restart;
use super::turn_ledger::TurnRecord;
use super::{Agent, Stop, Turn, TurnFailure};
use crate::compactor::{Compacted, encode_checkpoint, restore_checkpoint};
use crate::execution_memory::{ToolStep, ended_turn, logged_steps};
use crate::model_response_recovery::DEFAULT_MAX_PROVIDER_ATTEMPTS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ending {
    Replied,
    Stopped(TurnStop),
    Discarded,
    Paused,
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
        self.pending_interruptions.clear();
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

    fn note_recorded(&mut self, turn: &Turn, ending: Ending, saved: bool) {
        let record = match ending {
            Ending::Discarded | Ending::Paused if saved && turn.compaction.checkpointed() => {
                TurnRecord::LogOnly
            }
            Ending::Discarded | Ending::Paused => return,
            _ if saved => TurnRecord::Saved,
            _ => TurnRecord::Unsaved,
        };
        self.ledger.push(record);
    }

    pub(super) fn save_turn(
        &mut self,
        prompt: &str,
        turn: &Turn,
        ending: Ending,
    ) -> Result<(), LogFailure> {
        let files = self.turn_files(turn, ending);
        let recorded = self.record_turn(prompt, turn, ending, &files);
        self.settle_steering(turn.start);
        self.note_recorded(turn, ending, recorded.is_ok());
        self.send_file_evidence(turn, &files);
        recorded
    }

    fn turn_files(&self, turn: &Turn, ending: Ending) -> Vec<FileEvidence> {
        if ending == Ending::Discarded || turn.start >= self.history.len() {
            return Vec::new();
        }
        let parsed = ended_turn(
            &self.history,
            turn.start,
            self.history.len(),
            turn.stop.trailing,
        );
        let steps = logged_steps(&parsed.steps, &turn.raw_outputs);
        turn.earlier_files.turn_files(&steps)
    }

    pub(super) fn keep_compacted_files(&self, turn: &mut Turn, covered: usize) {
        let parsed = ended_turn(
            &self.history,
            turn.start,
            self.history.len(),
            turn.stop.trailing,
        );
        let steps = logged_steps(&parsed.steps, &turn.raw_outputs);
        let covered = covered.min(steps.len());
        turn.earlier_files.keep_compacted(&steps[..covered]);
    }

    fn send_file_evidence(&mut self, turn: &Turn, files: &[FileEvidence]) {
        let start = turn.start;
        if files.is_empty() || start >= self.history.len() {
            return;
        }
        let parsed = ended_turn(&self.history, start, self.history.len(), turn.stop.trailing);
        let at = parsed.steps.last().map_or(start + 1, ToolStep::end);
        self.history
            .insert(at, ChatMessage::user(file_evidence_context(files)));
    }

    fn record_turn(
        &mut self,
        prompt: &str,
        turn: &Turn,
        ending: Ending,
        files: &[FileEvidence],
    ) -> Result<(), LogFailure> {
        let start = turn.start;
        let Some(log) = self.log.as_mut().filter(|_| ending != Ending::Paused) else {
            return Ok(());
        };
        let parsed = (ending != Ending::Discarded && start < self.history.len())
            .then(|| ended_turn(&self.history, start, self.history.len(), turn.stop.trailing));
        let logged = match (parsed, ending) {
            (Some(parsed), Ending::Replied) => HistoryTurn {
                user: parsed.user,
                steps: logged_steps(&parsed.steps, &turn.raw_outputs),
                steering: parsed.logged_steering(),
                files,
                end: TurnEnd::Replied {
                    text: parsed.reply,
                    provider_replay: parsed.reply_replay,
                },
            },
            (Some(parsed), Ending::Stopped(reason)) => HistoryTurn {
                user: parsed.user,
                steps: logged_steps(&parsed.steps, &turn.raw_outputs),
                steering: parsed.logged_steering(),
                files,
                end: TurnEnd::Stopped {
                    reason,
                    partial: parsed.reply,
                },
            },
            _ => HistoryTurn {
                user: prompt,
                steps: Vec::new(),
                steering: Vec::new(),
                files: &[],
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
        source: &str,
    ) -> Result<(), LogFailure> {
        let Some(log) = self.log.as_ref() else {
            return Ok(());
        };
        if turn.start >= self.history.len() {
            return Ok(());
        }
        let mut point_turn = turn_so_far(&self.history, turn);
        let files = turn.earlier_files.turn_files(&point_turn.steps);
        point_turn.files = &files;
        log.record_recovery(&RecoveryPoint {
            turn_id: turn.id,
            turn: point_turn,
            source,
            cause,
            progress,
            tool_state: turn.tool_evidence.saved(),
            model: &self.config.model,
            requested_fast_mode: self.config.fast_mode,
            fast_mode: turn.fast_mode,
            ultrafast_mode: self.config.ultrafast_mode,
            attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
            consumed_attempts,
        })
    }

    pub(super) fn record_wait(
        &self,
        turn: &Turn,
        cause: ModelRecoveryCause,
        action: ModelRecoveryAction,
        consumed_attempts: usize,
        restart: &Restart<'_>,
    ) -> Result<(), Stop> {
        self.record_recovery(
            turn,
            cause,
            RecoveryProgress::Waiting(action),
            consumed_attempts,
            restart.source(&turn.language.stage),
        )
        .map_err(|failure| Stop::Failed {
            failure: TurnFailure::Persistence(failure),
            partial: restart.partial().to_owned(),
        })
    }

    pub(super) fn discard_recovery(&self) {
        if let Some(log) = self.log.as_ref() {
            let _ = log.clear_recovery();
        }
    }
}

fn turn_so_far<'a>(history: &'a [ChatMessage], turn: &Turn) -> HistoryTurn<'a> {
    let parsed = ended_turn(history, turn.start, history.len(), turn.stop.trailing);
    HistoryTurn {
        user: parsed.user,
        steps: logged_steps(&parsed.steps, &turn.raw_outputs),
        steering: parsed.logged_steering(),
        files: &[],
        end: TurnEnd::Replied {
            text: "",
            provider_replay: None,
        },
    }
}
