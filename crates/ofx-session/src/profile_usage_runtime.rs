use std::collections::HashSet;
use std::path::Path;

use ofx_contract::{
    UsageCompleteness, UsageIncident, UsageReport, UsageReportError, UsageScope,
    build_rolling_report,
};

use crate::profile_usage_store::{LoadedUsage, ProfileUsageStore, UsageStoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProfileUsageError {
    #[error(transparent)]
    Store(#[from] UsageStoreError),
    #[error(transparent)]
    Report(#[from] UsageReportError),
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
    ) -> Result<UsageReport, ProfileUsageError> {
        let loaded = self.store.load()?;
        Ok(build_report(&loaded, scope, snapshot_time_ms)?)
    }
}

fn build_report(
    loaded: &LoadedUsage,
    scope: UsageScope,
    snapshot_time_ms: i64,
) -> Result<UsageReport, UsageReportError> {
    let durable_fact_ids: HashSet<&str> =
        loaded.facts.iter().map(|fact| fact.id.as_str()).collect();
    let mut unknown_pending = false;
    let mut incidents = loaded.incidents.clone();
    for marker in &loaded.pending {
        if durable_fact_ids.contains(marker.id.as_str()) {
            continue;
        }
        if marker.observed_at_ms >= snapshot_time_ms {
            unknown_pending = true;
            continue;
        }
        incidents.push(UsageIncident {
            occurred_at_ms: marker.observed_at_ms,
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
        loaded.coverage_started_at_ms,
        &loaded.facts,
        &incidents,
    )
}

#[cfg(test)]
mod tests;
