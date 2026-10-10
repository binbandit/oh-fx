use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ofx_contract::{
    GenerationFact, UsageCompleteness, UsageIncident, UsageReport, UsageReportError, UsageScope,
    build_rolling_report,
};

use crate::profile_usage_store::{
    AppendOutcome, LoadedUsage, ProfileEvent, ProfileUsageStore, UsageStoreError,
};
use crate::usage_recovery::UsageRecovery;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProfileUsageError {
    #[error(transparent)]
    Store(#[from] UsageStoreError),
    #[error(transparent)]
    Report(#[from] UsageReportError),
    #[error("ConflictingUsagePublication")]
    Conflict,
}

impl ProfileUsageError {
    pub(crate) fn ledger_unavailable(self) -> bool {
        matches!(
            self,
            Self::Store(UsageStoreError::LockBusy | UsageStoreError::LockAbandoned)
        )
    }
}

#[derive(Clone)]
pub struct ProfilePublisher {
    store: Arc<Mutex<ProfileUsageStore>>,
    abandoned: Arc<AtomicBool>,
}

impl ProfilePublisher {
    pub fn open(data_dir: &Path) -> Option<Self> {
        let store = ProfileUsageStore::open(data_dir).ok()?;
        Some(Self {
            abandoned: store.abandon_flag(),
            store: Arc::new(Mutex::new(store)),
        })
    }

    pub fn abandon_for_process_exit(&self) {
        self.abandoned.store(true, Ordering::Release);
    }

    pub(crate) fn publish(&self, event: ProfileEvent<'_>) -> Result<(), ProfileUsageError> {
        let outcome = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .append_event(event)?;
        match outcome {
            AppendOutcome::Conflict => Err(ProfileUsageError::Conflict),
            AppendOutcome::Appended | AppendOutcome::Duplicate => Ok(()),
        }
    }
}

pub struct ProfileUsage {
    store: ProfileUsageStore,
}

impl ProfileUsage {
    pub fn open(data_dir: &Path) -> Result<Self, ProfileUsageError> {
        Ok(Self {
            store: ProfileUsageStore::open(data_dir)?,
        })
    }

    pub fn report(
        &mut self,
        scope: UsageScope,
        snapshot_time_ms: i64,
        recovery: &UsageRecovery,
    ) -> Result<UsageReport, ProfileUsageError> {
        let loaded = self.store.load()?;
        Ok(build_report(&loaded, scope, snapshot_time_ms, recovery)?)
    }
}

fn build_report(
    loaded: &LoadedUsage,
    scope: UsageScope,
    snapshot_time_ms: i64,
    recovery: &UsageRecovery,
) -> Result<UsageReport, UsageReportError> {
    let recovered_times = recovery
        .facts
        .iter()
        .map(|fact| fact.created_at_ms)
        .chain(
            recovery
                .incidents
                .iter()
                .map(|incident| incident.occurred_at_ms),
        )
        .chain(recovery.pending.iter().map(|marker| marker.observed_at_ms));
    let coverage_started_at_ms = recovered_times
        .fold(loaded.coverage_started_at_ms, |started, at| {
            Some(started.map_or(at, |started| started.min(at)))
        });
    let facts: Vec<GenerationFact> = loaded
        .facts
        .iter()
        .chain(&recovery.facts)
        .cloned()
        .collect();
    let durable_fact_ids: HashSet<&str> =
        loaded.facts.iter().map(|fact| fact.id.as_str()).collect();
    let mut unknown_pending = recovery.unknown_pending;
    let mut incidents = loaded.incidents.clone();
    incidents.extend_from_slice(&recovery.incidents);
    let awaited = recovery
        .facts
        .iter()
        .map(|fact| (fact.id.as_str(), fact.created_at_ms))
        .chain(
            loaded
                .pending
                .iter()
                .chain(&recovery.pending)
                .map(|marker| (marker.id.as_str(), marker.observed_at_ms)),
        );
    for (id, at) in awaited {
        if durable_fact_ids.contains(id) {
            continue;
        }
        if at >= snapshot_time_ms {
            unknown_pending = true;
            continue;
        }
        incidents.push(UsageIncident {
            occurred_at_ms: at,
            completeness: UsageCompleteness::Pending,
        });
    }
    if unknown_pending {
        incidents.push(UsageIncident {
            occurred_at_ms: snapshot_time_ms.saturating_sub(1).max(0),
            completeness: UsageCompleteness::Incomplete,
        });
    }
    build_rolling_report(
        scope,
        snapshot_time_ms,
        coverage_started_at_ms,
        &facts,
        &incidents,
    )
}

#[cfg(test)]
mod tests;
