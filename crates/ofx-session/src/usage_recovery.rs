use std::path::Path;

use ofx_config::PrivateDir;
use ofx_contract::{GenerationFact, PendingMarker, UsageCompleteness, UsageIncident};

use crate::session_error::SessionError;
use crate::session_log::read_metadata;
use crate::session_store::SESSIONS_DIR;
use crate::session_usage::{Availability, UsageSnapshot};
use crate::session_usage_sidecar::{SIDECAR_FILE, load_conversation};
use crate::usage_recovery_registry::{MarkedSession, checkpoint_modified_at_ns, marked_sessions};

const MAX_RECOVERY_RECORDS: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageRecovery {
    pub(crate) facts: Vec<GenerationFact>,
    pub(crate) incidents: Vec<UsageIncident>,
    pub(crate) pending: Vec<PendingMarker>,
    pub(crate) unknown_pending: bool,
}

impl UsageRecovery {
    pub fn collect(data_dir: &Path) -> Self {
        Self::collect_marked(data_dir).unwrap_or_else(|_| Self {
            unknown_pending: true,
            ..Self::default()
        })
    }

    fn collect_marked(data_dir: &Path) -> Result<Self, SessionError> {
        let mut recovery = Self::default();
        let Some(data) = PrivateDir::open_existing(data_dir)? else {
            return Ok(recovery);
        };
        let marked = marked_sessions(&data)?;
        if marked.is_empty() {
            return Ok(recovery);
        }
        let sessions = data.open_child(SESSIONS_DIR)?;
        for session in &marked {
            match read_marked(sessions.as_ref(), session) {
                Some((usage, updated_at_ms, newer)) => recovery.add(&usage, updated_at_ms, newer),
                None => recovery.unknown_pending = true,
            }
        }
        Ok(recovery)
    }

    fn add(&mut self, usage: &UsageSnapshot, updated_at_ms: i64, newer: bool) {
        if !newer {
            self.unknown_pending = true;
        }
        if !usage.needs_profile_recovery() {
            return;
        }
        let settled = usage.settled_through_sequence == usage.next_sequence.saturating_sub(1);
        if !settled {
            self.unknown_pending = true;
        }
        let observed_at_ms = updated_at_ms.max(0);
        let full = bounded(&mut self.facts, usage.publication_backlog.iter().cloned())
            | bounded(&mut self.incidents, usage.incidents.iter().copied())
            | bounded(
                &mut self.pending,
                usage.pending.iter().map(|pending| PendingMarker {
                    id: pending.id.clone(),
                    observed_at_ms: pending.observed_at_ms.unwrap_or(observed_at_ms),
                }),
            );
        let unexplained =
            usage.billing == Availability::Incomplete && usage.incidents.is_empty() && settled;
        let gap = UsageIncident {
            occurred_at_ms: observed_at_ms,
            completeness: UsageCompleteness::Incomplete,
        };
        let full = full || (unexplained && bounded(&mut self.incidents, [gap]));
        self.unknown_pending |= full;
    }
}

fn bounded<T>(records: &mut Vec<T>, added: impl IntoIterator<Item = T>) -> bool {
    for record in added {
        if records.len() == MAX_RECOVERY_RECORDS {
            return true;
        }
        records.push(record);
    }
    false
}

fn read_marked(
    sessions: Option<&PrivateDir>,
    marked: &MarkedSession,
) -> Option<(UsageSnapshot, i64, bool)> {
    let dir = sessions?.open_child(&marked.id).ok()??;
    let metadata = read_metadata(&dir, &marked.id).ok()?;
    let usage = load_conversation(&dir, &marked.id, metadata.updated_at_ms).ok()?;
    let checkpoint = checkpoint_modified_at_ns(&dir, SIDECAR_FILE);
    let newer = match checkpoint {
        Some(modified) => modified > marked.marker_modified_at_ns,
        None => {
            usage.needs_profile_recovery()
                && metadata.updated_at_ms >= marked.protected_updated_at_ms
        }
    };
    Some((usage, metadata.updated_at_ms, newer))
}

#[cfg(test)]
mod tests;
