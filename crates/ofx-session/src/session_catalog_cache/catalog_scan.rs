use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use ofx_config::PrivateDir;

use super::fingerprint::{Fingerprint, fingerprint};
use super::{CachedCatalog, Reuse, Row, RowSummary, save_catalog};
use crate::session_discovery::classify_session;
use crate::session_summary_codec::SessionSummary;

const WORKERS: usize = 4;

#[derive(Debug, Default)]
pub(crate) struct CatalogScan {
    pub(crate) summaries: Vec<SessionSummary>,
    pub(crate) skipped_invalid: usize,
}

struct Entry {
    id: String,
    fingerprint: Option<Fingerprint>,
    listed: Option<SessionSummary>,
}

impl Entry {
    fn row(&self) -> Option<Row> {
        Some(Row {
            id: self.id.clone(),
            fingerprint: self.fingerprint?,
            summary: self.listed.as_ref().map(RowSummary::of),
        })
    }
}

#[derive(Default)]
struct Observations {
    entries: Vec<Entry>,
    skipped_invalid: usize,
    changed: bool,
}

pub(crate) fn scan_catalog(sessions: &PrivateDir, names: &[String], writable: bool) -> CatalogScan {
    let cached = CachedCatalog::load(sessions);
    let next = AtomicUsize::new(0);
    let observed = thread::scope(|scope| {
        let helpers: Vec<_> = (1..WORKERS.min(names.len()))
            .map(|_| scope.spawn(|| observe_all(sessions, names, &next, &cached)))
            .collect();
        let mut observed = observe_all(sessions, names, &next, &cached);
        for helper in helpers {
            let found = helper
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            observed.entries.extend(found.entries);
            observed.skipped_invalid += found.skipped_invalid;
            observed.changed |= found.changed;
        }
        observed
    });
    let rows: Vec<Row> = observed.entries.iter().filter_map(Entry::row).collect();
    if writable && (observed.changed || !cached.present || rows.len() != cached.count()) {
        save_catalog(sessions, rows);
    }
    CatalogScan {
        summaries: observed
            .entries
            .into_iter()
            .filter_map(|entry| entry.listed)
            .collect(),
        skipped_invalid: observed.skipped_invalid,
    }
}

fn observe_all(
    sessions: &PrivateDir,
    names: &[String],
    next: &AtomicUsize,
    cached: &CachedCatalog,
) -> Observations {
    let mut observed = Observations::default();
    while let Some(id) = names.get(next.fetch_add(1, Ordering::Relaxed)) {
        let before = fingerprint(sessions, id);
        if let Some(stamp) = before
            && let Some(reused) = cached.reuse(id, &stamp)
        {
            observed.entries.push(Entry {
                id: id.clone(),
                fingerprint: Some(stamp),
                listed: match reused {
                    Reuse::Listed(summary) => Some(summary),
                    Reuse::Excluded => None,
                },
            });
            continue;
        }
        let Ok(summary) = classify_session(sessions, id) else {
            observed.skipped_invalid += 1;
            continue;
        };
        let after = RowSummary::of(&summary)
            .persistable()
            .then(|| fingerprint(sessions, id))
            .flatten();
        let stable = before.is_some() && before == after;
        observed.changed |= stable || cached.contains(id);
        observed.entries.push(Entry {
            id: id.clone(),
            fingerprint: after.filter(|_| stable),
            listed: Some(summary),
        });
    }
    observed
}
