use ofx_config::ProviderId;
use ofx_contract::{
    GenerationFact, ProviderBilling, UsageCompleteness, UsageIncident, valid_gateway_generation_id,
};
use ofx_text::lowercase_hex;
use sha2::{Digest, Sha256};

use super::{
    Availability, MAX_IDENTIFIER_BYTES, MAX_PENDING_GENERATIONS, MAX_PUBLICATION_BACKLOG,
    PendingGeneration, UsageSnapshot, UsageSnapshotError,
};
use crate::session_codec::SavedProvider;

const CANONICAL_DIGEST_BYTES: usize = 13;

pub(super) fn exact_fact(
    provider: &SavedProvider,
    billing: &ProviderBilling,
) -> Option<GenerationFact> {
    let fact = GenerationFact {
        id: canonical_exact_generation_id(provider, &billing.generation_id)?,
        created_at_ms: billing.created_at_ms,
        model: billing.model.clone(),
        input_tokens: billing.input_tokens,
        output_tokens: billing.output_tokens,
        cache_read_tokens: billing.cache_read_tokens,
        cache_write_tokens: billing.cache_write_tokens,
        reasoning_tokens: billing.reasoning_tokens,
        billable_web_search_calls: billing.billable_web_search_calls,
        total_cost: billing.total_cost,
    };
    fact.is_valid().then_some(fact)
}

fn canonical_exact_generation_id(provider: &SavedProvider, external_id: &str) -> Option<String> {
    if *provider.id() == ProviderId::Gateway {
        return valid_gateway_generation_id(external_id).then(|| external_id.to_owned());
    }
    if external_id.is_empty()
        || external_id.len() > MAX_IDENTIFIER_BYTES
        || external_id.bytes().any(|byte| byte.is_ascii_control())
    {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(provider.id().label());
    if let Some(binding) = provider.binding() {
        hash.update(binding);
    }
    hash.update([0]);
    hash.update(external_id);
    let digest = hash.finalize();
    let encoded = lowercase_hex(&digest[..CANONICAL_DIGEST_BYTES]).to_ascii_uppercase();
    Some(format!("gen_{encoded}"))
}

fn exact_usage_origin(provider: &ProviderId) -> &'static str {
    match provider {
        ProviderId::Gateway => "exact/gateway",
        ProviderId::Codex => "exact/codex",
        ProviderId::Grok => "exact/grok",
        ProviderId::Configured(_) => "exact/configured",
    }
}

impl UsageSnapshot {
    pub(super) fn observe_exact(
        &mut self,
        sequence: u64,
        id: &str,
        provider: &ProviderId,
        observed_at_ms: i64,
    ) -> Result<(), UsageSnapshotError> {
        let origin = exact_usage_origin(provider);
        let known = self
            .pending
            .iter()
            .find(|pending| pending.sequence == sequence || pending.id == id);
        if let Some(pending) = known {
            let same = pending.id == id
                && pending.sequence == sequence
                && pending.provider == *provider
                && pending.origin == origin
                && pending.team.is_none()
                && pending.credential_source.is_none()
                && pending.credential_identity.is_none()
                && pending.account_id.is_none();
            return self.refuse_unless(same, UsageSnapshotError::Invalid);
        }
        let identifier_bytes = self
            .identifier_bytes()
            .saturating_add(id.len())
            .saturating_add(origin.len());
        self.refuse_unless(
            self.pending.len() < MAX_PENDING_GENERATIONS
                && identifier_bytes <= MAX_IDENTIFIER_BYTES,
            UsageSnapshotError::CapacityExceeded,
        )?;
        self.pending.push(PendingGeneration {
            id: id.to_owned(),
            sequence,
            provider: provider.clone(),
            origin: origin.to_owned(),
            team: None,
            credential_source: None,
            credential_identity: None,
            account_id: None,
            observed_at_ms: Some(observed_at_ms),
        });
        if self.billing == Availability::Complete {
            self.billing = Availability::Pending;
        }
        Ok(())
    }

    pub(super) fn stage_publication(
        &mut self,
        fact: GenerationFact,
    ) -> Result<(), UsageSnapshotError> {
        if !self.pending.iter().any(|pending| pending.id == fact.id) {
            return Ok(());
        }
        let refusal = match self
            .publication_backlog
            .iter()
            .find(|existing| existing.id == fact.id)
        {
            Some(existing) if *existing == fact => return Ok(()),
            Some(_) => UsageSnapshotError::Invalid,
            None if self.publication_backlog.len() == MAX_PUBLICATION_BACKLOG => {
                UsageSnapshotError::CapacityExceeded
            }
            None => {
                self.publication_backlog.push(fact);
                return Ok(());
            }
        };
        self.billing = Availability::Incomplete;
        self.push_incident(UsageIncident {
            occurred_at_ms: fact.created_at_ms,
            completeness: UsageCompleteness::Incomplete,
        });
        Err(refusal)
    }

    fn refuse_unless(
        &mut self,
        accepted: bool,
        refusal: UsageSnapshotError,
    ) -> Result<(), UsageSnapshotError> {
        if accepted {
            return Ok(());
        }
        self.billing = Availability::Incomplete;
        Err(refusal)
    }
}

#[cfg(test)]
mod tests;
