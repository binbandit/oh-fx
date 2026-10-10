use super::RecordIndex;
use super::records::{ProfileEvent, push_coverage};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const RETENTION_MS: i64 = 35 * DAY_MS;
const COMPACTION_SLACK_MS: i64 = DAY_MS;
pub(super) const COMPACTION_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

pub(super) fn retention_cutoff(now_ms: i64) -> i64 {
    now_ms.checked_sub(RETENTION_MS).unwrap_or(0)
}

impl RecordIndex {
    pub(super) fn has_expired(&self, now_ms: i64) -> bool {
        let cutoff = retention_cutoff(now_ms);
        self.loaded
            .facts
            .iter()
            .any(|fact| fact.created_at_ms < cutoff)
            || self
                .loaded
                .pending
                .iter()
                .any(|marker| marker.observed_at_ms < cutoff || self.resolved(&marker.id))
            || self
                .loaded
                .incidents
                .iter()
                .any(|incident| incident.occurred_at_ms < cutoff)
    }

    pub(super) fn has_aged(&self, now_ms: i64) -> bool {
        let cutoff = retention_cutoff(now_ms)
            .checked_sub(COMPACTION_SLACK_MS)
            .unwrap_or(0);
        self.loaded
            .facts
            .iter()
            .any(|fact| fact.created_at_ms < cutoff)
            || self
                .loaded
                .pending
                .iter()
                .any(|marker| marker.observed_at_ms < cutoff)
            || self
                .loaded
                .incidents
                .iter()
                .any(|incident| incident.occurred_at_ms < cutoff)
    }

    pub(super) fn retained_count(&self, now_ms: i64) -> usize {
        usize::from(self.loaded.coverage_started_at_ms.is_some()) + self.retained(now_ms).count()
    }

    pub(super) fn retained_lines(&self, now_ms: i64) -> String {
        let mut out = String::new();
        if let Some(started_at_ms) = self.loaded.coverage_started_at_ms {
            push_coverage(&mut out, started_at_ms);
        }
        for event in self.retained(now_ms) {
            out.push_str(&event.line());
        }
        out
    }

    fn retained(&self, now_ms: i64) -> impl Iterator<Item = ProfileEvent<'_>> {
        let cutoff = retention_cutoff(now_ms);
        let facts = self
            .loaded
            .facts
            .iter()
            .filter(move |fact| fact.created_at_ms >= cutoff)
            .map(ProfileEvent::Generation);
        let pending = self
            .loaded
            .pending
            .iter()
            .filter(move |marker| marker.observed_at_ms >= cutoff && !self.resolved(&marker.id))
            .map(ProfileEvent::Pending);
        let incidents = self
            .loaded
            .incidents
            .iter()
            .filter(move |incident| incident.occurred_at_ms >= cutoff)
            .map(ProfileEvent::Incident);
        facts.chain(pending).chain(incidents)
    }

    fn resolved(&self, id: &str) -> bool {
        self.fact_variants.contains_key(id)
    }
}
