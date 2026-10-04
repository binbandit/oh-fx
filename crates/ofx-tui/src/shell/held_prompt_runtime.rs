use ofx_contract::UiCommand;

use super::{ActiveTurn, Shell, SubmissionState};

impl Shell<'_> {
    pub(super) fn prompt_held(&mut self) {
        if let Some(submission) = self
            .outstanding
            .iter_mut()
            .find(|submission| submission.state == SubmissionState::Active)
            .filter(|submission| submission.turn_id.is_none())
        {
            submission.state = SubmissionState::Held;
            self.turn = None;
        }
    }

    pub(super) fn holds_prompt(&self) -> bool {
        self.outstanding
            .iter()
            .any(|submission| submission.state == SubmissionState::Held)
    }

    pub(super) fn held_prompt_dropped(&mut self) {
        self.outstanding
            .retain(|submission| submission.state != SubmissionState::Held);
        self.promote_next();
    }

    pub(super) fn drop_held_prompt(&mut self) -> bool {
        if !self.holds_prompt() {
            return false;
        }
        self.held_prompt_dropped();
        self.send(UiCommand::DropHeldPrompt);
        true
    }

    pub(super) fn resume_held_prompt(&mut self, index: usize) {
        let now_ms = self.now_ms();
        let submission = &mut self.outstanding[index];
        submission.state = SubmissionState::Active;
        let mut turn = ActiveTurn::new(&submission.prompt, now_ms);
        turn.turn_id = submission.turn_id;
        self.turn = Some(turn);
    }
}

#[cfg(test)]
mod tests;
