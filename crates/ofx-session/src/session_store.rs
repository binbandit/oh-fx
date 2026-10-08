use std::io::Read;
use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

use ofx_config::PrivateDir;
use ofx_text::lowercase_hex;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use crate::session_catalog_cache::{CatalogScan, catalog_file_exists, scan_catalog};
use crate::session_children::{ChildSessions, has_owner_marker};
use crate::session_codec::{DEFAULT_CONVERSATION_LANGUAGE, SessionMetadata, SessionPreferences};
use crate::session_error::SessionError;
use crate::session_layout::{generate_session_id, is_valid_session_id};
use crate::session_log::managed_file::{
    file_type, has_private_dir_mode, permissions, private_file_mode, session_directory_names,
};
use crate::session_log::{
    LOCK_DEADLINE, SavedSession, SessionDisposal, WritableSession, delete_session, load_session,
    now_ms, resume_session, start_session,
};
use crate::session_store_paths::{is_valid_workspace_root, normalize_workspace_root};
use crate::session_summary_codec::{
    ResumablePage, ResumeContinuation, SessionSummary, listed_page_from_summaries,
    resumable_page_from_summaries, sort_summaries_newest_first,
};

const SESSIONS_DIR: &str = "sessions";
const CONTINUE_DIR: &str = "continue";
const MAX_REMEMBERED_SESSION_BYTES: usize = 256;
const MAX_LATEST_SELECTION_RETRIES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeTarget {
    Last,
    Id(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListScope {
    AllWorkspaces,
    CurrentWorkspace,
}

pub struct SessionCatalog {
    summaries: Vec<SessionSummary>,
    skipped_invalid: usize,
    workspace_root: String,
}

impl SessionCatalog {
    pub fn summaries(&self) -> &[SessionSummary] {
        &self.summaries
    }

    pub fn listed_page(
        &self,
        scope: ListScope,
        continuation: Option<&ResumeContinuation>,
        limit: usize,
    ) -> ResumablePage {
        listed_page_from_summaries(&self.summaries, self.scope_root(scope), continuation, limit)
    }

    pub fn skipped_invalid(&self) -> usize {
        self.skipped_invalid
    }

    fn scope_root(&self, scope: ListScope) -> Option<&str> {
        match scope {
            ListScope::AllWorkspaces => None,
            ListScope::CurrentWorkspace => Some(self.workspace_root.as_str()),
        }
    }

    pub fn page(
        &self,
        scope: ListScope,
        active_id: Option<&str>,
        continuation: Option<&ResumeContinuation>,
        limit: usize,
    ) -> ResumablePage {
        resumable_page_from_summaries(
            &self.summaries,
            self.scope_root(scope),
            active_id,
            continuation,
            limit,
        )
    }
}

pub struct SessionStore {
    data: Option<PrivateDir>,
    sessions: Option<PrivateDir>,
    workspace_root: String,
    writable: bool,
    lock_deadline: Duration,
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
                subagent_child: false,
            },
        )
    }

    pub fn resume(&self, id: &str) -> Result<WritableSession, SessionError> {
        let mut session = self.open_within(id, self.lock_deadline)?;
        self.move_here(&mut session)?;
        Ok(session)
    }

    pub fn open_without_waiting(&self, id: &str) -> Result<WritableSession, SessionError> {
        self.open_within(id, Duration::ZERO)
    }

    pub fn move_here(&self, session: &mut WritableSession) -> Result<(), SessionError> {
        if session.metadata().workspace_root == self.workspace_root {
            return Ok(());
        }
        session.rebind_workspace(&self.workspace_root)
    }

    fn open_within(&self, id: &str, deadline: Duration) -> Result<WritableSession, SessionError> {
        let sessions = self
            .writable_sessions()
            .map_err(|_| SessionError::SessionNotFound)?;
        if let Some(dir) = sessions.open_child(id).ok().flatten()
            && has_owner_marker(&dir)?
        {
            return Err(SessionError::OneOffSessionNotResumable);
        }
        let session = resume_session(sessions, id, deadline)?;
        if session.metadata().subagent_child {
            return Err(SessionError::OneOffSessionNotResumable);
        }
        Ok(session)
    }

    pub fn children(&self, parent_id: &str) -> Result<ChildSessions, SessionError> {
        ChildSessions::new(
            self.writable_sessions()?.try_clone()?,
            parent_id,
            &self.workspace_root,
        )
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

    pub fn resume_target(&self, target: &ResumeTarget) -> Result<WritableSession, SessionError> {
        match target {
            ResumeTarget::Last => self.resume_latest(),
            ResumeTarget::Id(id) => self.resume(id),
        }
    }

    pub fn discard_pristine(&self, session: WritableSession) -> SessionDisposal {
        let owned_here = session.metadata().workspace_root == self.workspace_root;
        match self.writable_sessions() {
            Ok(sessions) if owned_here && session.is_pristine() => {
                delete_session(sessions, session)
            }
            _ => SessionDisposal::Retained,
        }
    }

    pub fn is_initialized(&self) -> bool {
        self.sessions.is_some()
    }

    pub fn load(&self, id: &str) -> Result<SavedSession, SessionError> {
        let sessions = self
            .sessions
            .as_ref()
            .ok_or(SessionError::SessionNotFound)?;
        load_session(sessions, id)
    }

    pub fn try_clone(&self) -> Result<Self, SessionError> {
        let clone = |dir: &Option<PrivateDir>| dir.as_ref().map(PrivateDir::try_clone).transpose();
        Ok(Self {
            data: clone(&self.data)?,
            sessions: clone(&self.sessions)?,
            workspace_root: self.workspace_root.clone(),
            writable: self.writable,
            lock_deadline: self.lock_deadline,
        })
    }

    pub fn has_catalog_index(&self) -> bool {
        self.sessions.as_ref().is_some_and(catalog_file_exists)
    }

    pub fn catalog(&self) -> Result<SessionCatalog, SessionError> {
        let scan = self.scan_summaries()?;
        Ok(SessionCatalog {
            summaries: scan.summaries,
            skipped_invalid: scan.skipped_invalid,
            workspace_root: self.workspace_root.clone(),
        })
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

    fn scan_summaries(&self) -> Result<CatalogScan, SessionError> {
        let Some(sessions) = &self.sessions else {
            return Ok(CatalogScan::default());
        };
        let names = session_directory_names(sessions)?;
        let mut scan = scan_catalog(sessions, &names, self.writable);
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
    lowercase_hex(&Sha256::digest(workspace_root.as_bytes()))
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
