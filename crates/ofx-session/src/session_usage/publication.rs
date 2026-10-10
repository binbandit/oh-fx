use ofx_contract::{GenerationFact, PendingMarker, UsageCompleteness, UsageIncident};

use super::{
    Availability, MAX_IDENTIFIER_BYTES, MAX_MODELS, ModelAggregate, UsageSnapshot,
    UsageSnapshotError,
};

pub(crate) struct PublicationBatch {
    pub(crate) pending: Vec<PendingMarker>,
    pub(crate) incidents: Vec<UsageIncident>,
    pub(crate) facts: Vec<GenerationFact>,
    pub(crate) checkpoint_changed: bool,
}

#[derive(Clone, Copy)]
struct Totals {
    total_cost: f64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: Option<u64>,
    request_count: Option<u64>,
    billable_web_search_calls: u64,
}

impl Totals {
    fn plus(&self, fact: &GenerationFact) -> Option<Self> {
        let total_cost = self.total_cost + fact.total_cost;
        Some(Self {
            total_cost: total_cost.is_finite().then_some(total_cost)?,
            input_tokens: self.input_tokens.checked_add(fact.input_tokens)?,
            output_tokens: self.output_tokens.checked_add(fact.output_tokens)?,
            cache_read_tokens: self.cache_read_tokens.checked_add(fact.cache_read_tokens)?,
            cache_write_tokens: self
                .cache_write_tokens
                .checked_add(fact.cache_write_tokens)?,
            reasoning_tokens: match (self.reasoning_tokens, fact.reasoning_tokens) {
                (Some(total), Some(count)) => Some(total.checked_add(count)?),
                _ => None,
            },
            request_count: match self.request_count {
                Some(requests) => Some(requests.checked_add(1)?),
                None => None,
            },
            billable_web_search_calls: self
                .billable_web_search_calls
                .checked_add(fact.billable_web_search_calls)?,
        })
    }
}

impl UsageSnapshot {
    pub(super) fn publication_batch(&mut self, now_ms: i64) -> PublicationBatch {
        let observed_at_ms = now_ms.max(0);
        let mut checkpoint_changed = false;
        let pending = self
            .pending
            .iter_mut()
            .map(|generation| {
                let observed = *generation.observed_at_ms.get_or_insert_with(|| {
                    checkpoint_changed = true;
                    observed_at_ms
                });
                PendingMarker {
                    id: generation.id.clone(),
                    observed_at_ms: observed,
                }
            })
            .collect();
        PublicationBatch {
            pending,
            incidents: self.incidents.clone(),
            facts: self.publication_backlog.clone(),
            checkpoint_changed,
        }
    }

    pub(super) fn remove_incident(&mut self, incident: &UsageIncident) {
        if let Some(index) = self
            .incidents
            .iter()
            .position(|existing| existing == incident)
        {
            self.incidents.remove(index);
        }
    }

    pub(super) fn settle_published(&mut self, fact: &GenerationFact) {
        if self.apply_published(fact).is_err() {
            self.billing = Availability::Incomplete;
            self.push_incident(UsageIncident {
                occurred_at_ms: fact.created_at_ms,
                completeness: UsageCompleteness::Incomplete,
            });
            self.remove_backlog(&fact.id);
        }
    }

    fn apply_published(&mut self, fact: &GenerationFact) -> Result<(), UsageSnapshotError> {
        let Some(pending_index) = self
            .pending
            .iter()
            .position(|pending| pending.id == fact.id)
        else {
            self.remove_backlog(&fact.id);
            return Ok(());
        };
        let sequence = self.pending[pending_index].sequence;
        let model_index = self
            .models
            .iter()
            .position(|model| model.model == fact.model);
        if model_index.is_none() {
            let resolved = &self.pending[pending_index];
            let released = [
                resolved.id.len(),
                resolved.origin.len(),
                resolved.team.as_ref().map_or(0, String::len),
                resolved.account_id.as_ref().map_or(0, String::len),
            ]
            .into_iter()
            .fold(0, usize::saturating_add);
            let identifier_bytes = self
                .identifier_bytes()
                .saturating_sub(released)
                .checked_add(fact.model.len());
            if self.models.len() >= MAX_MODELS
                || identifier_bytes.is_none_or(|bytes| bytes > MAX_IDENTIFIER_BYTES)
            {
                self.billing = Availability::Incomplete;
                return Err(UsageSnapshotError::CapacityExceeded);
            }
        }
        let Some(totals) = self.totals().plus(fact) else {
            self.billing = Availability::Incomplete;
            return Err(UsageSnapshotError::CapacityExceeded);
        };
        if let Some(index) = model_index {
            self.models[index].add(fact, sequence)?;
        } else {
            let mut model = ModelAggregate {
                model: fact.model.clone(),
                first_sequence: sequence,
                total_cost: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: Some(0),
                request_count: Some(0),
                billable_web_search_calls: 0,
            };
            model.add(fact, sequence)?;
            self.models.push(model);
        }
        self.models.sort_by_key(|model| model.first_sequence);
        self.set_totals(totals);
        self.pending.remove(pending_index);
        self.remove_backlog(&fact.id);
        if self.billing == Availability::Pending && self.pending.is_empty() {
            self.billing = Availability::Complete;
        }
        Ok(())
    }

    fn remove_backlog(&mut self, id: &str) {
        if let Some(index) = self
            .publication_backlog
            .iter()
            .position(|fact| fact.id == id)
        {
            self.publication_backlog.remove(index);
        }
    }

    fn totals(&self) -> Totals {
        Totals {
            total_cost: self.total_cost,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            reasoning_tokens: self.reasoning_tokens,
            request_count: self.request_count,
            billable_web_search_calls: self.billable_web_search_calls,
        }
    }

    fn set_totals(&mut self, totals: Totals) {
        self.total_cost = totals.total_cost;
        self.input_tokens = totals.input_tokens;
        self.output_tokens = totals.output_tokens;
        self.cache_read_tokens = totals.cache_read_tokens;
        self.cache_write_tokens = totals.cache_write_tokens;
        self.reasoning_tokens = totals.reasoning_tokens;
        self.request_count = totals.request_count;
        self.billable_web_search_calls = totals.billable_web_search_calls;
    }
}

impl ModelAggregate {
    fn add(&mut self, fact: &GenerationFact, sequence: u64) -> Result<(), UsageSnapshotError> {
        let totals = Totals {
            total_cost: self.total_cost,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            reasoning_tokens: self.reasoning_tokens,
            request_count: self.request_count,
            billable_web_search_calls: self.billable_web_search_calls,
        }
        .plus(fact)
        .ok_or(UsageSnapshotError::CapacityExceeded)?;
        self.total_cost = totals.total_cost;
        self.input_tokens = totals.input_tokens;
        self.output_tokens = totals.output_tokens;
        self.cache_read_tokens = totals.cache_read_tokens;
        self.cache_write_tokens = totals.cache_write_tokens;
        self.reasoning_tokens = totals.reasoning_tokens;
        self.request_count = totals.request_count;
        self.billable_web_search_calls = totals.billable_web_search_calls;
        self.first_sequence = self.first_sequence.min(sequence);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
