use std::path::Path;

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

pub(crate) fn last_active(home: &Path, id: &str) -> Option<i64> {
    let sessions = open_sessions(home)?;
    let summary = classify_session(&sessions, id, Classification::Resume).ok()??;
    Some(summary.updated_at_ms)
}

#[cfg(test)]
mod tests;
