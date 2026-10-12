use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process;

use ofx_config::{DurableError, PrivateDir};
use ofx_trace::trace_log;

use super::doctor::{DoctorReport, doctor};
use super::listing::{list_items, record_error, store_error, summary_of};
use crate::fx_sessions::open_v2_root;
use crate::session_error::SessionError;
use crate::session_manager::{IoFault, Manager};
use crate::session_store::{SESSIONS_DIR, SessionCatalog, checked_workspace_root};
use crate::session_summary_codec::{SessionSource, SessionSummary, sort_summaries_newest_first};

const V2_DIR: &str = "v2";
const WIRING: &str = "wiring";

pub struct SessionsV2 {
    data_dir: PathBuf,
    home: PathBuf,
    workspace_root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreFault(IoFault);

impl fmt::Display for StoreFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl SessionsV2 {
    pub fn open_read_only(
        data_dir: &Path,
        home: &Path,
        workspace_root: &str,
    ) -> Result<Self, SessionError> {
        let workspace_root = checked_workspace_root(workspace_root)?;
        trace_log!(WIRING, "action=StoreOpen session=- pid={}", process::id());
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            home: home.to_path_buf(),
            workspace_root,
        })
    }

    pub fn doctor(&self, limit: usize) -> Result<DoctorReport, StoreFault> {
        match own_root(&self.data_dir).map_err(StoreFault)? {
            Some(own) => doctor(&Manager::new(own), limit).map_err(StoreFault),
            None => Ok(DoctorReport::default()),
        }
    }

    pub fn catalog(&self) -> Result<SessionCatalog, SessionError> {
        let own = own_root(&self.data_dir)
            .map_err(store_error)?
            .map(Manager::new);
        let mut summaries = match &own {
            Some(own) => list_items(own)
                .map_err(store_error)?
                .iter()
                .map(|item| summary_of(item, SessionSource::OhFx))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| record_error(&error))?,
            None => Vec::new(),
        };
        let stored: HashSet<String> = summaries.iter().map(|summary| summary.id.clone()).collect();
        summaries.extend(
            self.fx_summaries()
                .into_iter()
                .filter(|summary| !stored.contains(&summary.id)),
        );
        sort_summaries_newest_first(&mut summaries);
        Ok(SessionCatalog::listed(
            summaries,
            self.workspace_root.clone(),
        ))
    }

    fn fx_summaries(&self) -> Vec<SessionSummary> {
        let Some(fx) = open_v2_root(&self.home).map(Manager::new) else {
            return Vec::new();
        };
        list_items(&fx)
            .unwrap_or_default()
            .iter()
            .filter_map(|item| summary_of(item, SessionSource::Fx).ok())
            .collect()
    }
}

impl Drop for SessionsV2 {
    fn drop(&mut self) {
        trace_log!(WIRING, "action=StoreClose session=- pid={}", process::id());
    }
}

fn own_root(data_dir: &Path) -> Result<Option<PrivateDir>, IoFault> {
    let opened = PrivateDir::open_existing(data_dir)
        .and_then(|data| match data {
            Some(data) => data.open_child(SESSIONS_DIR),
            None => Ok(None),
        })
        .and_then(|sessions| match sessions {
            Some(sessions) => sessions.open_child(V2_DIR),
            None => Ok(None),
        });
    opened.map_err(|error| match error {
        DurableError::PathUnsafe | DurableError::AccessDenied => IoFault::AccessDenied,
        _ => IoFault::Io,
    })
}

#[cfg(test)]
mod tests;
