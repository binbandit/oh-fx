mod import;
mod remembered;

use std::collections::HashSet;
use std::path::Path;

use ofx_config::PrivateDir;

use crate::session_catalog_cache::{CatalogIndex, scan_catalog};
use crate::session_discovery::Classification;
use crate::session_error::SessionError;
use crate::session_log::managed_file::{has_private_dir_mode, session_directory_names};
use crate::session_log::{SessionArchive, load_archive};
use crate::session_summary_codec::{SessionSource, SessionSummary, sort_summaries_newest_first};

pub(crate) use import::{ImportSource, Imported, seal, untouched_import};
pub(crate) use remembered::{remembered_session_id, resumed_at};

const PROFILE_DIR: &str = ".fx";
const SESSIONS_DIR: &str = "sessions";

pub struct FxSessions {
    sessions: Option<PrivateDir>,
}

impl FxSessions {
    pub fn open(home: &Path) -> Self {
        Self {
            sessions: open_sessions(home),
        }
    }

    pub(crate) fn merge_into(&self, summaries: &mut Vec<SessionSummary>, stored: &[String]) {
        let listed = self.summaries();
        if listed.is_empty() {
            return;
        }
        let stored: HashSet<&str> = stored.iter().map(String::as_str).collect();
        summaries.extend(
            listed
                .into_iter()
                .filter(|summary| !stored.contains(summary.id.as_str())),
        );
        sort_summaries_newest_first(summaries);
    }

    pub(crate) fn session_dir(&self, id: &str) -> Option<PrivateDir> {
        self.sessions
            .as_ref()
            .and_then(|sessions| sessions.open_child(id).ok().flatten())
    }

    pub(crate) fn archive(&self, id: &str) -> Result<SessionArchive, SessionError> {
        let dir = self.session_dir(id).ok_or(SessionError::SessionNotFound)?;
        let mut archive = load_archive(&dir, id, SessionError::FxSessionUnreadable).map_err(
            |error| match error {
                SessionError::SessionNotFound => error,
                _ => SessionError::FxSessionUnreadable,
            },
        )?;
        archive.source = SessionSource::Fx;
        Ok(archive)
    }

    fn summaries(&self) -> Vec<SessionSummary> {
        let Some(sessions) = &self.sessions else {
            return Vec::new();
        };
        let Ok(names) = session_directory_names(sessions) else {
            return Vec::new();
        };
        let mut summaries = scan_catalog(
            sessions,
            &names,
            CatalogIndex::Bypassed,
            Classification::Resume,
        )
        .summaries;
        for summary in &mut summaries {
            summary.source = SessionSource::Fx;
        }
        summaries
    }
}

pub(crate) fn import_from_fx(
    home: &Path,
    sessions: &PrivateDir,
    id: &str,
) -> Result<Option<Imported>, SessionError> {
    match sessions.open_child(id) {
        Ok(None) => match open_sessions(home) {
            Some(fx) => import::import(&fx, sessions, id),
            None => Ok(None),
        },
        Ok(Some(copy)) => match (import::read_marker(&copy), open_sessions(home)) {
            (Some(marker), Some(fx)) => import::refresh(&fx, sessions, &copy, id, &marker),
            _ => Ok(None),
        },
        Err(_) => Ok(None),
    }
}

fn open_profile(home: &Path) -> Option<PrivateDir> {
    if !home.is_absolute() {
        return None;
    }
    let profile = PrivateDir::open_existing(&home.join(PROFILE_DIR)).ok()??;
    has_private_dir_mode(&profile).ok()?.then_some(profile)
}

fn open_sessions(home: &Path) -> Option<PrivateDir> {
    let sessions = open_profile(home)?.open_child(SESSIONS_DIR).ok()??;
    has_private_dir_mode(&sessions).ok()?.then_some(sessions)
}

#[cfg(test)]
mod tests;
