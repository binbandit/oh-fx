use std::path::Path;

use ofx_config::PrivateDir;

use super::import::outdated;
use super::{open_profile, open_sessions};
use crate::session_discovery::{Classification, classify_session};
use crate::session_log::managed_file::has_private_dir_mode;
use crate::session_store::{read_remembered_session, remembered_file_name};

const CONTINUE_DIR: &str = "continue";

pub(crate) fn remembered_session_id(home: &Path, workspace_root: &str) -> Option<String> {
    let directory = open_profile(home)?.open_child(CONTINUE_DIR).ok()??;
    if !has_private_dir_mode(&directory).ok()? {
        return None;
    }
    read_remembered_session(&directory, &remembered_file_name(workspace_root)).ok()?
}

pub(crate) fn resumed_at(home: &Path, sessions: &PrivateDir, id: &str) -> Option<i64> {
    let fx = open_sessions(home);
    let copy = match sessions.open_child(id) {
        Ok(Some(copy)) => copy,
        Ok(None) => return last_active(&fx?, id),
        Err(_) => return None,
    };
    fx.filter(|fx| outdated(fx, &copy, id))
        .and_then(|fx| last_active(&fx, id))
        .or_else(|| last_active(sessions, id))
}

fn last_active(sessions: &PrivateDir, id: &str) -> Option<i64> {
    let summary = classify_session(sessions, id, Classification::Resume).ok()??;
    Some(summary.updated_at_ms)
}

#[cfg(test)]
mod tests;
