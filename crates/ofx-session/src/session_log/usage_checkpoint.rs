use std::time::Instant;

use ofx_config::DurableError;
use ofx_contract::{DeliveryOutcome, FileChangeStats, ProviderBilling, RequestTicket};

use super::{WritableSession, now_ms};
use crate::session_codec::SavedProvider;
use crate::session_error::SessionError;
use crate::session_usage::ReserveFailure;
use crate::session_usage_sidecar;

impl WritableSession {
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
        session_usage_sidecar::write(&self.owned.dir, &self.metadata.id, &snapshot)
    }
}

fn elapsed_ms(ticket: RequestTicket) -> u64 {
    u64::try_from(ticket.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}
