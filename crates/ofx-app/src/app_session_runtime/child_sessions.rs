use std::sync::{Arc, Mutex, PoisonError};

use ofx_agent::{ChildRecord, ChildSettings, ChildStore};
use ofx_contract::{ConversationLog, LogFailure};
use ofx_session::{
    ChildSessions, SavedProvider, SessionError, SessionLog, SessionPreferences, WritableSession,
};

pub(super) struct SessionChildren {
    sessions: ChildSessions,
    provider: SavedProvider,
    language: String,
}

struct ChildSession {
    session: Arc<Mutex<WritableSession>>,
    provider: SavedProvider,
}

impl SessionChildren {
    pub(super) fn new(sessions: ChildSessions, provider: SavedProvider, language: String) -> Self {
        Self {
            sessions,
            provider,
            language,
        }
    }
}

impl ChildStore for SessionChildren {
    fn parent_id(&self) -> &str {
        self.sessions.parent_id()
    }

    fn new_child_id(&self) -> Result<String, LogFailure> {
        self.sessions.new_child_id().map_err(failure)
    }

    fn save_registry(&self, registry: &[u8]) -> Result<(), LogFailure> {
        self.sessions.save_registry(registry).map_err(failure)
    }

    fn start_child(
        &self,
        child_id: &str,
        settings: &ChildSettings,
    ) -> Result<Arc<dyn ChildRecord>, LogFailure> {
        let preferences = SessionPreferences {
            provider: self.provider.clone(),
            model: settings.model.clone(),
            effort: settings.effort.clone(),
            fast_mode: settings.fast_mode,
        };
        let session = self
            .sessions
            .start(child_id, preferences, &self.language)
            .map_err(failure)?;
        Ok(Arc::new(ChildSession {
            session: Arc::new(Mutex::new(session)),
            provider: self.provider.clone(),
        }))
    }
}

impl ChildRecord for ChildSession {
    fn begin_work(&self, work_id: &str) {
        self.session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .begin_work(work_id);
    }

    fn log(&self) -> Box<dyn ConversationLog> {
        Box::new(SessionLog::new(
            Arc::clone(&self.session),
            self.provider.clone(),
        ))
    }
}

fn failure(error: SessionError) -> LogFailure {
    LogFailure {
        code: error.to_string(),
    }
}
