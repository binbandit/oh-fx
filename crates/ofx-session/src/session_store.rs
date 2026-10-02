use std::fmt::Write as _;
use std::io::Read;
use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

use ofx_config::PrivateDir;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use crate::session_codec::{DEFAULT_CONVERSATION_LANGUAGE, SessionMetadata, SessionPreferences};
use crate::session_discovery::classify_session;
use crate::session_error::SessionError;
use crate::session_layout::{generate_session_id, is_valid_session_id};
use crate::session_log::managed_file::{
    file_type, has_private_dir_mode, permissions, private_file_mode, session_directory_names,
};
use crate::session_log::{
    LOCK_DEADLINE, SavedSession, WritableSession, load_session, now_ms, resume_session,
    start_session,
};
use crate::session_store_paths::{is_valid_workspace_root, normalize_workspace_root};
use crate::session_summary_codec::{
    ResumablePage, ResumeContinuation, SessionSummary, resumable_page_from_summaries,
    sort_summaries_newest_first,
};

const SESSIONS_DIR: &str = "sessions";
const CONTINUE_DIR: &str = "continue";
const MAX_REMEMBERED_SESSION_BYTES: usize = 256;
const MAX_LATEST_SELECTION_RETRIES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListScope {
    AllWorkspaces,
    CurrentWorkspace,
}

pub struct SessionStore {
    data: Option<PrivateDir>,
    sessions: Option<PrivateDir>,
    workspace_root: String,
    writable: bool,
    lock_deadline: Duration,
}

struct SummaryScan {
    summaries: Vec<SessionSummary>,
    skipped_invalid: usize,
}

impl SessionStore {
    pub fn open(data_dir: &Path, workspace_root: &str) -> Result<Self, SessionError> {
        let workspace_root = checked_workspace_root(workspace_root)?;
        let data = PrivateDir::open_or_create(data_dir).map_err(layout_error)?;
        let sessions = data
            .open_or_create_child(SESSIONS_DIR)
            .map_err(layout_error)?;
        Ok(Self {
            data: Some(data),
            sessions: Some(sessions),
            workspace_root,
            writable: true,
            lock_deadline: LOCK_DEADLINE,
        })
    }

    pub fn open_read_only(data_dir: &Path, workspace_root: &str) -> Result<Self, SessionError> {
        let workspace_root = checked_workspace_root(workspace_root)?;
        let data = PrivateDir::open_existing(data_dir)?;
        let sessions = match &data {
            Some(data) => data.open_child(SESSIONS_DIR)?,
            None => None,
        };
        Ok(Self {
            data,
            sessions,
            workspace_root,
            writable: false,
            lock_deadline: LOCK_DEADLINE,
        })
    }

    pub fn start(&self, preferences: SessionPreferences) -> Result<WritableSession, SessionError> {
        let sessions = self.writable_sessions()?;
        let id = generate_session_id().ok_or(SessionError::SessionStartFailed)?;
        let now = now_ms();
        start_session(
            sessions,
            SessionMetadata {
                id,
                origin_workspace_root: self.workspace_root.clone(),
                workspace_root: self.workspace_root.clone(),
                created_at_ms: now,
                updated_at_ms: now,
                conversation_language: DEFAULT_CONVERSATION_LANGUAGE.to_owned(),
                preferences,
                title: None,
            },
        )
    }

    pub fn resume(&self, id: &str) -> Result<WritableSession, SessionError> {
        let sessions = self
            .writable_sessions()
            .map_err(|_| SessionError::SessionNotFound)?;
        let mut session = resume_session(sessions, id, self.lock_deadline)?;
        if session.metadata().workspace_root != self.workspace_root {
            session.rebind_workspace(&self.workspace_root)?;
        }
        Ok(session)
    }

    pub fn resume_latest(&self) -> Result<WritableSession, SessionError> {
        let sessions = self
            .writable_sessions()
            .map_err(|_| SessionError::SessionNotFound)?;
        let scan = self.scan_summaries()?;
        let mut vanished = 0;
        for summary in &scan.summaries {
            if summary.workspace_root != self.workspace_root {
                continue;
            }
            let opened =
                resume_session(sessions, &summary.id, self.lock_deadline).and_then(|session| {
                    if session.metadata().workspace_root == self.workspace_root {
                        Ok(session)
                    } else {
                        Err(SessionError::SessionTargetChanged)
                    }
                });
            match opened {
                Ok(session) => return Ok(session),
                Err(SessionError::SessionNotFound | SessionError::SessionTargetChanged) => {
                    vanished += 1;
                    if vanished == MAX_LATEST_SELECTION_RETRIES {
                        return opened;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        if scan.skipped_invalid > 0 {
            Err(SessionError::NoReadableSessions)
        } else {
            Err(SessionError::NoSavedSessions)
        }
    }

    pub fn load(&self, id: &str) -> Result<SavedSession, SessionError> {
        let sessions = self
            .sessions
            .as_ref()
            .ok_or(SessionError::SessionNotFound)?;
        load_session(sessions, id)
    }

    pub fn resumable_page(
        &self,
        scope: ListScope,
        active_id: Option<&str>,
        continuation: Option<&ResumeContinuation>,
        limit: usize,
    ) -> Result<ResumablePage, SessionError> {
        let scan = self.scan_summaries()?;
        let workspace_root = match scope {
            ListScope::AllWorkspaces => None,
            ListScope::CurrentWorkspace => Some(self.workspace_root.as_str()),
        };
        Ok(resumable_page_from_summaries(
            &scan.summaries,
            workspace_root,
            active_id,
            continuation,
            limit,
        ))
    }

    pub fn remembered_session_id(&self) -> Result<Option<String>, SessionError> {
        let Some(data) = &self.data else {
            return Ok(None);
        };
        if !has_private_dir_mode(data)? {
            return Err(SessionError::SessionPathUnsafe);
        }
        let Some(directory) = data.open_child(CONTINUE_DIR)? else {
            return Ok(None);
        };
        if !has_private_dir_mode(&directory)? {
            return Err(SessionError::SessionPathUnsafe);
        }
        read_remembered_session(&directory, &remembered_file_name(&self.workspace_root))
    }

    pub fn remember_session_id(&self, id: &str) -> Result<(), SessionError> {
        if !self.writable {
            return Err(SessionError::SessionStoreReadOnly);
        }
        if !is_valid_session_id(id) {
            return Err(SessionError::InvalidSessionId);
        }
        let data = self
            .data
            .as_ref()
            .ok_or(SessionError::SessionStoreUnavailable)?;
        if !has_private_dir_mode(data)? {
            return Err(SessionError::SessionPathUnsafe);
        }
        let directory = data.open_or_create_child(CONTINUE_DIR)?;
        directory.replace(
            &remembered_file_name(&self.workspace_root),
            format!("{id}\n").as_bytes(),
        )?;
        Ok(())
    }

    fn writable_sessions(&self) -> Result<&PrivateDir, SessionError> {
        match &self.sessions {
            Some(sessions) if self.writable => Ok(sessions),
            _ => Err(SessionError::SessionStoreUnavailable),
        }
    }

    fn scan_summaries(&self) -> Result<SummaryScan, SessionError> {
        let mut scan = SummaryScan {
            summaries: Vec::new(),
            skipped_invalid: 0,
        };
        let Some(sessions) = &self.sessions else {
            return Ok(scan);
        };
        for name in session_directory_names(sessions)? {
            match classify_session(sessions, &name) {
                Ok(summary) => scan.summaries.push(summary),
                Err(_) => scan.skipped_invalid += 1,
            }
        }
        sort_summaries_newest_first(&mut scan.summaries);
        Ok(scan)
    }
}

fn checked_workspace_root(workspace_root: &str) -> Result<String, SessionError> {
    let normalized = normalize_workspace_root(workspace_root);
    if is_valid_workspace_root(normalized) {
        Ok(normalized.to_owned())
    } else {
        Err(SessionError::InvalidWorkspaceRoot)
    }
}

fn layout_error(error: ofx_config::DurableError) -> SessionError {
    match SessionError::from(error) {
        SessionError::Storage(_) => SessionError::DurableLayoutFailed,
        other => other,
    }
}

fn remembered_file_name(workspace_root: &str) -> String {
    let digest = Sha256::digest(workspace_root.as_bytes());
    let mut name = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(name, "{byte:02x}");
    }
    name
}

fn read_remembered_session(
    directory: &PrivateDir,
    name: &str,
) -> Result<Option<String>, SessionError> {
    match fs::statat(directory.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if file_type(&stat) == FileType::RegularFile => {}
        Ok(_) => return Err(SessionError::InvalidRememberedSession),
        Err(Errno::NOENT) => return Ok(None),
        Err(errno) => return Err(errno.into()),
    }
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = match fs::openat(directory.as_fd(), name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::ISDIR | Errno::NOTDIR | Errno::NXIO) => {
            return Err(SessionError::InvalidRememberedSession);
        }
        Err(errno) => return Err(errno.into()),
    };
    let stat = fs::fstat(&fd)?;
    let max_bytes = u64::try_from(MAX_REMEMBERED_SESSION_BYTES).unwrap_or(u64::MAX);
    if file_type(&stat) != FileType::RegularFile
        || stat.st_nlink > 1
        || permissions(&stat) != private_file_mode()
        || u64::try_from(stat.st_size).map_or(true, |size| size > max_bytes)
    {
        return Err(SessionError::InvalidRememberedSession);
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    parse_remembered_session_id(&bytes).map(|id| Some(id.to_owned()))
}

fn parse_remembered_session_id(bytes: &[u8]) -> Result<&str, SessionError> {
    if bytes.len() < 2 || bytes.len() > MAX_REMEMBERED_SESSION_BYTES || bytes.last() != Some(&b'\n')
    {
        return Err(SessionError::InvalidRememberedSession);
    }
    std::str::from_utf8(&bytes[..bytes.len() - 1])
        .ok()
        .filter(|id| is_valid_session_id(id))
        .ok_or(SessionError::InvalidRememberedSession)
}

#[cfg(test)]
mod tests;
