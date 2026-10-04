mod catalog_codec;
mod catalog_scan;
mod fingerprint;

use std::collections::{HashMap, HashSet};
use std::io::Read;

use ofx_config::PrivateDir;
use rustix::fs::Mode;

use crate::session_codec::{MAX_SESSION_TITLE_BYTES, is_valid_conversation_language};
use crate::session_log::managed_file::{Access, open_managed_file, permissions};
use crate::session_summary_codec::SessionSummary;

use catalog_codec::{decode_catalog, encode_catalog};
pub(crate) use catalog_scan::{CatalogScan, scan_catalog};
use fingerprint::{Fingerprint, fingerprint};

const CATALOG_FILE: &str = ".resume-catalog";
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 100_000;
const DISPLAY_METADATA_FLAG: u8 = 1;
const CHECKPOINT_FLAG: u8 = 1 << 1;
const MANAGED_CHILDREN_FLAG: u8 = 1 << 2;
const KNOWN_FLAGS: u8 = DISPLAY_METADATA_FLAG | CHECKPOINT_FLAG | MANAGED_CHILDREN_FLAG;
#[cfg(target_os = "macos")]
const MAX_PATH_BYTES: usize = 1024;
#[cfg(not(target_os = "macos"))]
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RowSummary {
    workspace_root: Option<String>,
    origin_workspace_root: Option<String>,
    title: Option<String>,
    preview: Option<String>,
    flags: u8,
    created_at_ms: i64,
    updated_at_ms: i64,
    history_len: u64,
    language: String,
}

impl RowSummary {
    fn of(summary: &SessionSummary) -> Self {
        Self {
            workspace_root: Some(summary.workspace_root.clone()),
            origin_workspace_root: Some(summary.origin_workspace_root.clone()),
            title: summary.title.clone(),
            preview: None,
            flags: if summary.has_checkpoint {
                CHECKPOINT_FLAG
            } else {
                0
            },
            created_at_ms: summary.created_at_ms,
            updated_at_ms: summary.updated_at_ms,
            history_len: u64::try_from(summary.history_len).unwrap_or(u64::MAX),
            language: summary.conversation_language.clone(),
        }
    }

    fn persistable(&self) -> bool {
        self.created_at_ms >= 0
            && self.updated_at_ms >= self.created_at_ms
            && is_valid_conversation_language(&self.language)
            && usize::try_from(self.history_len).is_ok()
            && self
                .title
                .as_ref()
                .is_none_or(|title| title.len() <= MAX_SESSION_TITLE_BYTES)
            && [&self.workspace_root, &self.origin_workspace_root]
                .into_iter()
                .flatten()
                .all(|root| root.starts_with('/') && root.len() <= MAX_PATH_BYTES)
    }

    fn listed(&self, id: &str) -> Option<SessionSummary> {
        if self.preview.is_some() || self.flags & !CHECKPOINT_FLAG != 0 {
            return None;
        }
        Some(SessionSummary {
            id: id.to_owned(),
            workspace_root: self.workspace_root.clone()?,
            origin_workspace_root: self.origin_workspace_root.clone()?,
            title: self.title.clone(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            conversation_language: self.language.clone(),
            history_len: usize::try_from(self.history_len).ok()?,
            has_checkpoint: self.flags & CHECKPOINT_FLAG != 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    id: String,
    fingerprint: Fingerprint,
    summary: Option<RowSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Reuse {
    Listed(SessionSummary),
    Excluded,
}

#[derive(Debug, Default)]
struct CachedCatalog {
    rows: Vec<Row>,
    index: HashMap<String, usize>,
    present: bool,
}

impl CachedCatalog {
    fn load(sessions: &PrivateDir) -> Self {
        read_catalog(sessions)
            .and_then(|bytes| decode_catalog(&bytes))
            .and_then(Self::indexed)
            .unwrap_or_default()
    }

    fn indexed(rows: Vec<Row>) -> Option<Self> {
        let mut index = HashMap::with_capacity(rows.len());
        for (position, row) in rows.iter().enumerate() {
            if index.insert(row.id.clone(), position).is_some() {
                return None;
            }
        }
        Some(Self {
            rows,
            index,
            present: true,
        })
    }

    fn count(&self) -> usize {
        self.index.len()
    }

    fn row(&self, id: &str) -> Option<&Row> {
        Some(&self.rows[*self.index.get(id)?])
    }

    fn reuse(&self, id: &str, stamp: &Fingerprint) -> Option<Reuse> {
        let row = self.row(id)?;
        if &row.fingerprint != stamp {
            return None;
        }
        match &row.summary {
            Some(summary) => summary.listed(id).map(Reuse::Listed),
            None => Some(Reuse::Excluded),
        }
    }
}

fn read_catalog(sessions: &PrivateDir) -> Option<Vec<u8>> {
    let file = open_managed_file(sessions, CATALOG_FILE, Access::ReadOnly).ok()??;
    let stat = rustix::fs::fstat(&file).ok()?;
    let size = usize::try_from(stat.st_size).ok()?;
    if permissions(&stat).intersects(Mode::RWXG | Mode::RWXO) || size > MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(size);
    file.take(u64::try_from(size).ok()?)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() == size).then_some(bytes)
}

fn save_catalog(sessions: &PrivateDir, fresh: Vec<Row>) {
    let previous = CachedCatalog::load(sessions);
    let replaced: HashSet<String> = fresh.iter().map(|row| row.id.clone()).collect();
    let mut rows = fresh;
    rows.extend(previous.rows.into_iter().filter(|row| {
        !replaced.contains(&row.id) && fingerprint(sessions, &row.id) == Some(row.fingerprint)
    }));
    if let Some(bytes) = encode_catalog(&rows) {
        let _ = sessions.replace(CATALOG_FILE, &bytes);
    }
}

#[cfg(test)]
mod tests;
