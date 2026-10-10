use ofx_contract::{
    DeliveryOutcome, FileChangeStats, ProviderBilling, UsageCompleteness, UsageIncident,
};

use super::exact::exact_fact;
use super::{Availability, UsageSnapshot};
use crate::session_codec::SavedProvider;

const MAX_ACTIVE_INVOCATIONS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReserveFailure {
    CapacityExceeded,
    SequenceOverflow,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Usage {
    state: UsageSnapshot,
    active: Vec<u64>,
    active_started_at_ms: Option<i64>,
}

impl Usage {
    pub(crate) fn fresh() -> Self {
        Self {
            state: UsageSnapshot::fresh(),
            active: Vec::new(),
            active_started_at_ms: None,
        }
    }

    pub(crate) fn restore(
        mut snapshot: UsageSnapshot,
        session_started_at_ms: i64,
        now_ms: i64,
    ) -> Self {
        snapshot.wall_duration_ms = 0;
        let active_started_at_ms = if session_started_at_ms <= 0 || session_started_at_ms > now_ms {
            snapshot.wall_duration_complete = false;
            now_ms
        } else {
            session_started_at_ms
        };
        Self {
            state: snapshot,
            active: Vec::new(),
            active_started_at_ms: Some(active_started_at_ms),
        }
    }

    pub(crate) fn reserve(&mut self, now_ms: i64) -> Result<u64, ReserveFailure> {
        self.active_started_at_ms.get_or_insert(now_ms);
        let sequence = self.state.next_sequence;
        if self.active.len() == MAX_ACTIVE_INVOCATIONS {
            return Err(ReserveFailure::CapacityExceeded);
        }
        self.state.next_sequence = sequence
            .checked_add(1)
            .ok_or(ReserveFailure::SequenceOverflow)?;
        self.active.push(sequence);
        Ok(sequence)
    }

    pub(crate) fn finish(
        &mut self,
        sequence: u64,
        duration_ms: u64,
        outcome: DeliveryOutcome,
        now_ms: i64,
    ) -> bool {
        let finished = self.release(sequence, duration_ms);
        if finished && outcome != DeliveryOutcome::Unbilled {
            self.state.billing = Availability::Incomplete;
            self.record_incident(now_ms);
        }
        finished
    }

    pub(crate) fn finish_exact(
        &mut self,
        sequence: u64,
        duration_ms: u64,
        provider: &SavedProvider,
        billing: &ProviderBilling,
        now_ms: i64,
    ) {
        let Some(fact) = exact_fact(provider, billing) else {
            self.finish(
                sequence,
                duration_ms,
                DeliveryOutcome::PossiblyBilledWithoutIdentity,
                now_ms,
            );
            return;
        };
        if !self.release(sequence, duration_ms) {
            return;
        }
        let staged = self
            .state
            .observe_exact(sequence, &fact.id, provider.id(), now_ms)
            .and_then(|()| self.state.stage_publication(fact));
        if staged.is_err() {
            self.mark_billing_incomplete(now_ms);
        }
    }

    fn release(&mut self, sequence: u64, duration_ms: u64) -> bool {
        if sequence == 0 || sequence >= self.state.next_sequence {
            self.fail_api_duration();
            return false;
        }
        let Some(index) = self.active.iter().position(|active| *active == sequence) else {
            return false;
        };
        self.active.swap_remove(index);
        let Some(api_duration_ms) = self.state.api_duration_ms.checked_add(duration_ms) else {
            self.fail_api_duration();
            return false;
        };
        self.state.api_duration_ms = api_duration_ms;
        if self.active.is_empty() {
            self.state.settled_through_sequence = self.state.next_sequence - 1;
        }
        true
    }

    pub(crate) fn record_committed_lines(&mut self, change: FileChangeStats) {
        let added = self
            .state
            .lines_added
            .checked_add(u64::from(change.additions));
        let removed = self
            .state
            .lines_removed
            .checked_add(u64::from(change.deletions));
        match (added, removed) {
            (Some(added), Some(removed)) => {
                self.state.lines_added = added;
                self.state.lines_removed = removed;
            }
            (Some(added), None) => {
                self.state.lines_added = added;
                self.state.code_complete = false;
            }
            (None, _) => self.state.code_complete = false,
        }
    }

    pub(crate) fn mark_code_incomplete(&mut self) {
        self.state.code_complete = false;
    }

    pub(crate) fn mark_billing_incomplete(&mut self, now_ms: i64) {
        self.state.billing = Availability::Incomplete;
        self.record_incident(now_ms.max(0));
    }

    pub(crate) fn snapshot(&mut self, now_ms: i64) -> UsageSnapshot {
        let started_at_ms = *self.active_started_at_ms.get_or_insert(now_ms);
        let elapsed_ms = u64::try_from(now_ms.saturating_sub(started_at_ms)).unwrap_or(0);
        let wall_duration_ms = self.state.wall_duration_ms.saturating_add(elapsed_ms);
        let settled = self.active.is_empty();
        UsageSnapshot {
            billing: if settled {
                self.state.billing
            } else {
                Availability::Incomplete
            },
            api_duration_complete: self.state.api_duration_complete && settled,
            wall_duration_complete: self.state.wall_duration_complete
                && wall_duration_ms != u64::MAX,
            wall_duration_ms,
            ..self.state.clone()
        }
    }

    fn fail_api_duration(&mut self) {
        self.state.billing = Availability::Incomplete;
        self.state.api_duration_complete = false;
    }

    fn record_incident(&mut self, occurred_at_ms: i64) {
        if occurred_at_ms < 0 {
            return;
        }
        self.state.push_incident(UsageIncident {
            occurred_at_ms,
            completeness: UsageCompleteness::Incomplete,
        });
    }
}

#[cfg(test)]
mod tests;
