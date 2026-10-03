use ofx_contract::TurnId;

use super::{Shell, SubmissionState};
use crate::output::activity_status::TurnPhase;
use crate::render_engine::transcript_blocks::Entry;

impl Shell<'_> {
    pub(super) fn steering_applied(&mut self, turn_id: TurnId, prompt: u64, text: String) {
        let promoted = self
            .outstanding
            .iter()
            .position(|submission| submission.sequence == prompt)
            .and_then(|index| self.outstanding.remove(index))
            .is_some_and(|submission| submission.state == SubmissionState::Active);
        if promoted {
            self.retire_promoted_turn();
            return;
        }
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| turn.turn_id == Some(turn_id))
        else {
            return;
        };
        let mut events = Vec::new();
        turn.markdown.flush(&mut events);
        turn.step_break = None;
        turn.phase = TurnPhase::Thinking;
        if turn
            .recovery
            .as_ref()
            .is_some_and(|recovery| !recovery.is_recovered())
        {
            turn.recovery = None;
        }
        self.transcript.append_assistant(events, &self.theme);
        self.push_entry(Entry::UserTurn { text });
    }

    fn retire_promoted_turn(&mut self) {
        if self
            .turn
            .as_ref()
            .is_some_and(|turn| turn.turn_id.is_none())
        {
            self.turn = None;
            self.invalidate();
        }
        self.promote_next();
    }

    pub(super) fn retract_waiting_steer(&mut self) -> bool {
        if !self.composer.is_empty() || self.composer.browsing_history() {
            return false;
        }
        let Some((prompt, text)) = self
            .options
            .steering
            .as_ref()
            .and_then(|queue| queue.retract_newest())
        else {
            return false;
        };
        self.outstanding
            .retain(|submission| submission.sequence != prompt);
        self.composer.restore_text(text);
        self.invalidate();
        true
    }
}

#[cfg(test)]
mod tests;
