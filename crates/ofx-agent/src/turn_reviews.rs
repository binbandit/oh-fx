use ofx_contract::{ReviewVerdict, ToolCall, ToolCallId};
use sha2::{Digest, Sha256};

const MAX_TURN_REVIEW_HOLDS: usize = 64;
const MAX_TURN_UNAVAILABLE_ATTEMPTS: usize = 64;
const ACTION_ID_DOMAIN: &[u8] = b"fx.permission-action.v1\0";

type ActionId = [u8; 32];

#[derive(Debug, Default)]
pub(crate) struct TurnReviews {
    holds: Vec<(ActionId, ReviewVerdict)>,
    unavailable_attempts: Vec<ActionId>,
    unavailable_budget_exhausted: bool,
    held_results: Vec<(ToolCallId, String)>,
}

impl TurnReviews {
    pub(crate) fn cached(&self, call: &ToolCall) -> Option<ReviewVerdict> {
        let id = action_id(call);
        self.holds
            .iter()
            .find(|(held, _)| *held == id)
            .map(|(_, verdict)| verdict.clone())
    }

    pub(crate) fn attempt_available(&self, call: &ToolCall) -> bool {
        !self.unavailable_budget_exhausted && !self.unavailable_attempts.contains(&action_id(call))
    }

    pub(crate) fn remember(&mut self, call: &ToolCall, verdict: &ReviewVerdict) {
        match verdict {
            ReviewVerdict::Clear => {}
            ReviewVerdict::Unavailable(_) => {
                let id = action_id(call);
                if self.unavailable_budget_exhausted
                    || self.unavailable_attempts.contains(&id)
                    || self.unavailable_attempts.len() == MAX_TURN_UNAVAILABLE_ATTEMPTS
                {
                    return;
                }
                self.unavailable_attempts.push(id);
                self.unavailable_budget_exhausted =
                    self.unavailable_attempts.len() == MAX_TURN_UNAVAILABLE_ATTEMPTS;
            }
            ReviewVerdict::Caution(_) | ReviewVerdict::EvidenceIncomplete => {
                let id = action_id(call);
                if self.holds.len() == MAX_TURN_REVIEW_HOLDS
                    || self.holds.iter().any(|(held, _)| *held == id)
                {
                    return;
                }
                self.holds.push((id, verdict.clone()));
            }
        }
    }

    pub(crate) fn record_held_result(&mut self, call_id: &ToolCallId, content: &str) {
        self.held_results
            .push((call_id.clone(), content.to_owned()));
    }

    pub(crate) fn held_results(&self) -> &[(ToolCallId, String)] {
        &self.held_results
    }
}

fn action_id(call: &ToolCall) -> ActionId {
    let mut hasher = Sha256::new();
    hasher.update(ACTION_ID_DOMAIN);
    hasher.update(call.name.as_bytes());
    hasher.update(b"\0");
    hasher.update(call.arguments.as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests;
