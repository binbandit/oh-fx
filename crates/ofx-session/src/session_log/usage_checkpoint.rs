use std::time::Instant;

use ofx_config::DurableError;
use ofx_contract::{
    DeliveryOutcome, FileChangeStats, GenerationFact, ProviderBilling, RequestTicket, UsageIncident,
};

use super::{WritableSession, now_ms};
use crate::session_codec::SavedProvider;
use crate::session_error::SessionError;
use crate::session_usage::{PublicationBatch, ReserveFailure};
use crate::session_usage_sidecar;
use crate::usage_recovery_registry::RecoveryRegistry;

#[derive(Default)]
pub(crate) struct UsageRecoveryTracking {
    registry: Option<RecoveryRegistry>,
    marked: bool,
}

impl WritableSession {
    pub(crate) fn track_usage_recovery(&mut self, registry: RecoveryRegistry) {
        self.usage_recovery = UsageRecoveryTracking {
            registry: Some(registry),
            marked: self.usage.snapshot(now_ms()).needs_profile_recovery(),
        };
    }

    pub(crate) fn begin_request(&mut self) -> Result<RequestTicket, SessionError> {
        let now = now_ms();
        let sequence = self.usage.reserve(now).map_err(|failure| match failure {
            ReserveFailure::CapacityExceeded => SessionError::UsageCapacityExceeded,
            ReserveFailure::SequenceOverflow => SessionError::UsageSequenceOverflow,
        })?;
        if let Err(error) = self.checkpoint_usage(now) {
            self.usage
                .finish(sequence, 0, DeliveryOutcome::Unbilled, now);
            return Err(error);
        }
        Ok(RequestTicket {
            sequence,
            started_at: Instant::now(),
        })
    }

    pub(crate) fn finish_request(
        &mut self,
        ticket: RequestTicket,
        outcome: DeliveryOutcome,
    ) -> Result<(), SessionError> {
        let now = now_ms();
        self.usage
            .finish(ticket.sequence, elapsed_ms(ticket), outcome, now);
        self.checkpoint_usage_for_continuation(now)
    }

    pub(crate) fn finish_exact_request(
        &mut self,
        ticket: RequestTicket,
        billing: &ProviderBilling,
        provider: &SavedProvider,
    ) -> Result<(), SessionError> {
        let now = now_ms();
        self.usage
            .finish_exact(ticket.sequence, elapsed_ms(ticket), provider, billing, now);
        self.checkpoint_usage_for_continuation(now)
    }

    pub(crate) fn record_committed_lines(&mut self, change: FileChangeStats) {
        self.usage.record_committed_lines(change);
        if self.checkpoint_usage(now_ms()).is_err() {
            self.usage.mark_code_incomplete();
        }
    }

    pub(crate) fn usage_publication_batch(&mut self) -> PublicationBatch {
        self.usage.publication_batch(now_ms())
    }

    pub(crate) fn usage_incident_published(&mut self, incident: &UsageIncident) {
        self.usage.incident_published(incident);
    }

    pub(crate) fn usage_fact_published(&mut self, fact: &GenerationFact) {
        self.usage.fact_published(fact);
    }

    pub(crate) fn save_published_usage(&mut self) {
        let _ = self.checkpoint_usage_for_continuation(now_ms());
    }

    fn checkpoint_usage(&mut self, now: i64) -> Result<(), SessionError> {
        self.require_writable()?;
        self.write_usage(now)
    }

    fn checkpoint_usage_for_continuation(&mut self, now: i64) -> Result<(), SessionError> {
        let Err(error) = self.checkpoint_usage(now) else {
            return Ok(());
        };
        self.usage.mark_billing_incomplete(now);
        match error {
            SessionError::Storage(DurableError::PostRenameFailed) => {
                Err(SessionError::SessionPersistenceUncertain)
            }
            error if self.require_writable().is_err() => Err(error),
            _ => Ok(()),
        }
    }

    fn write_usage(&mut self, now: i64) -> Result<(), SessionError> {
        let snapshot = self.usage.snapshot(now);
        let owed = snapshot.needs_profile_recovery();
        let marked = self.usage_recovery.marked;
        if owed && let Some(registry) = &self.usage_recovery.registry {
            let updated_at_ms = self.metadata.updated_at_ms;
            let protected = if marked {
                updated_at_ms
            } else {
                now.max(updated_at_ms.saturating_add(1))
            };
            registry.mark(&self.metadata.id, protected, !marked)?;
        }
        session_usage_sidecar::write(&self.owned.dir, &self.metadata.id, &snapshot)?;
        if !owed && let Some(registry) = &self.usage_recovery.registry {
            registry.clear(&self.metadata.id)?;
        }
        self.usage_recovery.marked = owed;
        Ok(())
    }
}

fn elapsed_ms(ticket: RequestTicket) -> u64 {
    u64::try_from(ticket.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}
