use std::collections::HashSet;
use std::path::Path;

use ofx_config::PrivateDir;

use crate::session_catalog_cache::{CatalogIndex, scan_catalog};
use crate::session_discovery::Classification;
use crate::session_log::managed_file::{has_private_dir_mode, session_directory_names};
use crate::session_summary_codec::{SessionSource, SessionSummary, sort_summaries_newest_first};

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

fn open_sessions(home: &Path) -> Option<PrivateDir> {
    if !home.is_absolute() {
        return None;
    }
    let profile = PrivateDir::open_existing(&home.join(PROFILE_DIR)).ok()??;
    let sessions = profile.open_child(SESSIONS_DIR).ok()??;
    let private = has_private_dir_mode(&profile).ok()? && has_private_dir_mode(&sessions).ok()?;
    private.then_some(sessions)
}

#[cfg(test)]
mod tests;
