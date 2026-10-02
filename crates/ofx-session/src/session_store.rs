use std::path::Path;

use ofx_config::PrivateDir;

use crate::session_codec::{DEFAULT_CONVERSATION_LANGUAGE, SessionMetadata, SessionPreferences};
use crate::session_error::SessionError;
use crate::session_layout::generate_session_id;
use crate::session_log::{
    SavedSession, WritableSession, load_session, now_ms, resume_session, start_session,
};
use crate::session_store_paths::{is_valid_workspace_root, normalize_workspace_root};

const SESSIONS_DIR: &str = "sessions";

pub struct SessionStore {
    sessions: Option<PrivateDir>,
    workspace_root: String,
    writable: bool,
}

impl SessionStore {
    pub fn open(data_dir: &Path, workspace_root: &str) -> Result<Self, SessionError> {
        let workspace_root = checked_workspace_root(workspace_root)?;
        let data = PrivateDir::open_or_create(data_dir).map_err(layout_error)?;
        let sessions = data
            .open_or_create_child(SESSIONS_DIR)
            .map_err(layout_error)?;
        Ok(Self {
            sessions: Some(sessions),
            workspace_root,
            writable: true,
        })
    }

    pub fn open_read_only(data_dir: &Path, workspace_root: &str) -> Result<Self, SessionError> {
        let workspace_root = checked_workspace_root(workspace_root)?;
        let sessions = match PrivateDir::open_existing(data_dir)? {
            Some(data) => data.open_child(SESSIONS_DIR)?,
            None => None,
        };
        Ok(Self {
            sessions,
            workspace_root,
            writable: false,
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
        let mut session = resume_session(sessions, id)?;
        if session.metadata().workspace_root != self.workspace_root {
            session.rebind_workspace(&self.workspace_root)?;
        }
        Ok(session)
    }

    pub fn load(&self, id: &str) -> Result<SavedSession, SessionError> {
        let sessions = self
            .sessions
            .as_ref()
            .ok_or(SessionError::SessionNotFound)?;
        load_session(sessions, id)
    }

    fn writable_sessions(&self) -> Result<&PrivateDir, SessionError> {
        match &self.sessions {
            Some(sessions) if self.writable => Ok(sessions),
            _ => Err(SessionError::SessionStoreUnavailable),
        }
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

#[cfg(test)]
mod tests;
