use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ofx_config::{DurableError, PrivateDir};
use ofx_contract::{
    DeliveryOutcome, FileChangeStats, GenerationFact, ProviderBilling, RequestTicket, UsageIncident,
};

use super::{WritableSession, now_ms};
use crate::session_codec::SavedProvider;
use crate::session_error::SessionError;
use crate::session_usage::{PublicationBatch, ReserveFailure, UsageSnapshot};
use crate::session_usage_sidecar;

#[derive(Default)]
pub(crate) struct UsageCheckpoints {
    writer: Arc<Mutex<CheckpointWriter>>,
    issued: u64,
}

#[derive(Default)]
struct CheckpointWriter {
    written: u64,
}

pub(crate) struct PublishedUsage {
    writer: Arc<Mutex<CheckpointWriter>>,
    sequence: u64,
    dir: PrivateDir,
    id: String,
    snapshot: UsageSnapshot,
}

impl UsageCheckpoints {
    fn issue(&mut self) -> u64 {
        self.issued += 1;
        self.issued
    }
}

impl CheckpointWriter {
    fn write(
        &mut self,
        sequence: u64,
        dir: &PrivateDir,
        id: &str,
        snapshot: &UsageSnapshot,
    ) -> Result<(), SessionError> {
        self.save(sequence, || session_usage_sidecar::write(dir, id, snapshot))
    }

    fn save(
        &mut self,
        sequence: u64,
        write: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        if self.written > sequence {
            return Ok(());
        }
        let saved = write();
        if matches!(
            saved,
            Ok(()) | Err(SessionError::Storage(DurableError::PostRenameFailed))
        ) {
            self.written = sequence;
        }
        saved
    }
}

impl PublishedUsage {
    pub(crate) fn save(&self) -> Result<(), SessionError> {
        self.writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(self.sequence, &self.dir, &self.id, &self.snapshot)
    }
}

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

    pub(crate) fn usage_publication_batch(&mut self) -> PublicationBatch {
        self.usage.publication_batch(now_ms())
    }

    pub(crate) fn usage_incident_published(&mut self, incident: &UsageIncident) {
        self.usage.incident_published(incident);
    }

    pub(crate) fn usage_fact_published(&mut self, fact: &GenerationFact) {
        self.usage.fact_published(fact);
    }

    pub(crate) fn published_usage(&mut self) -> Option<PublishedUsage> {
        let now = now_ms();
        let dir = self
            .require_writable()
            .and_then(|()| Ok(self.owned.dir.try_clone()?));
        let Ok(dir) = dir else {
            self.usage.mark_billing_incomplete(now);
            return None;
        };
        Some(PublishedUsage {
            writer: Arc::clone(&self.usage_checkpoints.writer),
            sequence: self.usage_checkpoints.issue(),
            dir,
            id: self.metadata.id.clone(),
            snapshot: self.usage.snapshot(now),
        })
    }

    pub(crate) fn published_usage_unsaved(&mut self) {
        self.usage.mark_billing_incomplete(now_ms());
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
        let sequence = self.usage_checkpoints.issue();
        self.usage_checkpoints
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(sequence, &self.owned.dir, &self.metadata.id, &snapshot)
    }
}

fn elapsed_ms(ticket: RequestTicket) -> u64 {
    u64::try_from(ticket.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
