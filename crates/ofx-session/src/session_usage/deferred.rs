use ofx_config::ProviderId;
use ofx_contract::{ObservedGeneration, valid_gateway_generation_id};

use super::{
    Availability, MAX_IDENTIFIER_BYTES, MAX_ORIGIN_BYTES, MAX_PENDING_GENERATIONS, MAX_TEAM_BYTES,
    PendingGeneration, UsageSnapshot, printable_within,
};
use crate::session_codec::recovery_checkpoint::RouteCredential;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DeferredRefusal {
    #[error("InvalidGenerationId")]
    GenerationId,
    #[error("InvalidGatewayOrigin")]
    Origin,
    #[error("InvalidGatewayTeam")]
    Team,
    #[error("ConflictingGenerationIdentity")]
    Conflicting,
    #[error("UsageCapacityExceeded")]
    CapacityExceeded,
}

impl UsageSnapshot {
    pub(super) fn observe_deferred(
        &mut self,
        sequence: u64,
        provider: &ProviderId,
        generation: &ObservedGeneration,
        credential: RouteCredential,
        observed_at_ms: i64,
    ) -> Result<(), DeferredRefusal> {
        validate(generation)?;
        let observed = PendingGeneration {
            id: generation.id.clone(),
            sequence,
            provider: provider.clone(),
            origin: generation.origin.clone(),
            team: generation.team.clone(),
            credential_source: Some(credential.source()),
            credential_identity: credential.identity(),
            account_id: None,
            observed_at_ms: Some(observed_at_ms),
        };
        if let Some(known) = self
            .pending
            .iter()
            .find(|pending| pending.sequence == sequence || pending.id == observed.id)
        {
            let same = PendingGeneration {
                observed_at_ms: known.observed_at_ms,
                ..observed
            } == *known;
            return self.accept_if(same, DeferredRefusal::Conflicting);
        }
        let identifier_bytes = self
            .identifier_bytes()
            .saturating_add(observed.id.len())
            .saturating_add(observed.origin.len())
            .saturating_add(observed.team.as_ref().map_or(0, String::len));
        self.accept_if(
            self.pending.len() < MAX_PENDING_GENERATIONS
                && identifier_bytes <= MAX_IDENTIFIER_BYTES,
            DeferredRefusal::CapacityExceeded,
        )?;
        self.pending.push(observed);
        if self.billing == Availability::Complete {
            self.billing = Availability::Pending;
        }
        Ok(())
    }

    fn accept_if(
        &mut self,
        accepted: bool,
        refusal: DeferredRefusal,
    ) -> Result<(), DeferredRefusal> {
        if accepted {
            return Ok(());
        }
        self.billing = Availability::Incomplete;
        Err(refusal)
    }
}

fn validate(generation: &ObservedGeneration) -> Result<(), DeferredRefusal> {
    if !valid_gateway_generation_id(&generation.id) {
        return Err(DeferredRefusal::GenerationId);
    }
    if !printable_within(&generation.origin, MAX_ORIGIN_BYTES) {
        return Err(DeferredRefusal::Origin);
    }
    if !generation
        .team
        .as_deref()
        .is_none_or(|team| printable_within(team, MAX_TEAM_BYTES))
    {
        return Err(DeferredRefusal::Team);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
