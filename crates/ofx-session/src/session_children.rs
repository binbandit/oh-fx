use std::time::Duration;

use ofx_config::PrivateDir;

use crate::session_codec::{SessionMetadata, SessionPreferences};
use crate::session_error::SessionError;
use crate::session_layout::{generate_session_id, is_valid_session_id};
use crate::session_log::managed_file::{lock_with_deadline, remove_created_dir};
use crate::session_log::{
    LOCK_DEADLINE, WritableSession, now_ms, resume_session, start_session, work_reply,
};

pub(crate) const CONTROL_DIR: &str = "subagent";
const OWNER_FILE: &str = "owner.json";
const REGISTRY_FILE: &str = "children.json";
const REGISTRY_LOCK_FILE: &str = "children.lock";
const REGISTRY_LOCK_DEADLINE: Duration = Duration::from_secs(2);
const MAX_REGISTRY_BYTES: usize = 512 * 1024;
const MAX_OWNER_BYTES: usize = 4 * 1024;

pub struct ChildSessions {
    sessions: PrivateDir,
    parent_id: String,
    workspace_root: String,
}

impl ChildSessions {
    pub(crate) fn new(
        sessions: PrivateDir,
        parent_id: &str,
        workspace_root: &str,
    ) -> Result<Self, SessionError> {
        if !is_valid_session_id(parent_id) {
            return Err(SessionError::InvalidSessionId);
        }
        Ok(Self {
            sessions,
            parent_id: parent_id.to_owned(),
            workspace_root: workspace_root.to_owned(),
        })
    }

    pub fn parent_id(&self) -> &str {
        &self.parent_id
    }

    pub fn new_child_id(&self) -> Result<String, SessionError> {
        generate_session_id().ok_or(SessionError::SessionStartFailed)
    }

    pub fn start(
        &self,
        id: &str,
        preferences: SessionPreferences,
        conversation_language: &str,
    ) -> Result<WritableSession, SessionError> {
        let now = now_ms();
        let session = start_session(
            &self.sessions,
            SessionMetadata {
                id: id.to_owned(),
                origin_workspace_root: self.workspace_root.clone(),
                workspace_root: self.workspace_root.clone(),
                created_at_ms: now,
                updated_at_ms: now,
                conversation_language: conversation_language.to_owned(),
                preferences,
                title: None,
                subagent_child: true,
            },
        )?;
        match self.mark_owner(&session) {
            Ok(()) => Ok(session),
            Err(error) => {
                drop(session);
                remove_created_dir(&self.sessions, id);
                Err(error)
            }
        }
    }

    fn mark_owner(&self, session: &WritableSession) -> Result<(), SessionError> {
        let marker = serde_json::to_vec(&OwnerMarker {
            schema_version: 1,
            parent_id: &self.parent_id,
        })
        .map_err(|_| SessionError::SessionStartFailed)?;
        session.control_dir()?.replace(OWNER_FILE, &marker)?;
        Ok(())
    }

    pub fn resume(&self, id: &str) -> Result<WritableSession, SessionError> {
        let session = resume_session(&self.sessions, id, LOCK_DEADLINE)?;
        if session.metadata().subagent_child {
            Ok(session)
        } else {
            Err(SessionError::SessionNotFound)
        }
    }

    pub fn reply_for_work(&self, id: &str, work_id: &str) -> Result<Option<String>, SessionError> {
        work_reply(&self.sessions, id, work_id)
    }

    pub fn load_registry(&self) -> Result<Option<Vec<u8>>, SessionError> {
        let Some(control) = self.parent_dir()?.open_child(CONTROL_DIR)? else {
            return Ok(None);
        };
        Ok(control
            .read_private(REGISTRY_FILE, MAX_REGISTRY_BYTES)?
            .map(|bytes| bytes.to_vec()))
    }

    pub fn save_registry(&self, bytes: &[u8]) -> Result<(), SessionError> {
        if bytes.len() > MAX_REGISTRY_BYTES {
            return Err(SessionError::SessionCommitFailed);
        }
        let control = self.parent_dir()?.open_or_create_child(CONTROL_DIR)?;
        let _lock = lock_with_deadline(&control, REGISTRY_LOCK_FILE, REGISTRY_LOCK_DEADLINE)?
            .ok_or(SessionError::SessionBusy)?;
        control.replace(REGISTRY_FILE, bytes)?;
        Ok(())
    }

    fn parent_dir(&self) -> Result<PrivateDir, SessionError> {
        self.sessions
            .open_child(&self.parent_id)?
            .ok_or(SessionError::SessionNotFound)
    }
}

#[derive(serde::Serialize)]
struct OwnerMarker<'a> {
    schema_version: u8,
    parent_id: &'a str,
}

pub(crate) fn has_owner_marker(session_dir: &PrivateDir) -> Result<bool, SessionError> {
    let Some(control) = session_dir.open_child(CONTROL_DIR)? else {
        return Ok(false);
    };
    Ok(control.read_private(OWNER_FILE, MAX_OWNER_BYTES)?.is_some())
}

#[cfg(test)]
mod tests;
