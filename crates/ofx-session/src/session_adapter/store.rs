use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process;

use ofx_config::{DurableError, PrivateDir};
use ofx_trace::trace_log;

use super::codec::{RecordError, decode_language, decode_preferences, decode_title, decode_usage};
use super::doctor::{DoctorReport, doctor};
use super::listing::{list_items, record_error, record_failure, store_error, summary_of};
use super::replay::{ReplayError, detail_turns};
use crate::fx_sessions::open_v2_root;
use crate::session_error::SessionError;
use crate::session_log::SessionArchive;
use crate::session_manager::{IoFault, Manager, OpenError, Role, SetKey, State};
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

    pub fn archive(&self, id: &str) -> Result<SessionArchive, SessionError> {
        if let Some(own) = own_root(&self.data_dir)
            .map_err(store_error)?
            .map(Manager::new)
        {
            match archive_in(&own, id, SessionSource::OhFx) {
                Err(SessionError::SessionNotFound) => {}
                other => return other,
            }
        }
        let fx = open_v2_root(&self.home)
            .map(Manager::new)
            .ok_or(SessionError::SessionNotFound)?;
        archive_in(&fx, id, SessionSource::Fx).map_err(|error| match error {
            SessionError::SessionNotFound => error,
            _ => SessionError::FxSessionUnreadable,
        })
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

fn archive_in(
    manager: &Manager,
    id: &str,
    source: SessionSource,
) -> Result<SessionArchive, SessionError> {
    let peeked = manager.peek(id).map_err(open_error)?;
    if peeked.role != Role::Root {
        return Err(SessionError::SessionNotFound);
    }
    let state = &peeked.state;
    let conversation_language = settings(state).map_err(|error| record_error(&error))?;
    let turns = detail_turns(manager, id).map_err(|error| match error {
        ReplayError::Open(error) => open_error(error),
        ReplayError::Session(
            error @ (SessionError::InvalidSessionFormat | SessionError::SessionStoreUnavailable),
        ) => error,
        ReplayError::Session(error) => record_failure(&error.to_string()),
    })?;
    Ok(SessionArchive {
        id: id.to_owned(),
        created_at_ms: i64::try_from(peeked.created_ms).unwrap_or(0),
        updated_at_ms: i64::try_from(peeked.updated_ms).unwrap_or(0),
        conversation_language,
        turns,
        source,
    })
}

fn settings(state: &State) -> Result<String, RecordError> {
    let language = decode_language(state.setting(SetKey::Language))?;
    decode_title(state.setting(SetKey::Title))?;
    if let Some(raw) = state.setting(SetKey::Prefs) {
        decode_preferences(raw)?;
    }
    if let Some(raw) = state.setting(SetKey::Usage) {
        decode_usage(raw)?;
    }
    Ok(language)
}

fn open_error(error: OpenError) -> SessionError {
    match error {
        OpenError::NotFound => SessionError::SessionNotFound,
        OpenError::Corrupt => SessionError::InvalidSessionFormat,
        OpenError::Fault(fault) => store_error(fault),
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
