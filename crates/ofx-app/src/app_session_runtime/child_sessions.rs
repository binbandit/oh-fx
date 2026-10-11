use std::sync::{Arc, Mutex, PoisonError, Weak};

use ofx_agent::{ChildRecord, ChildSettings, ChildStore, ResumedChild};
use ofx_contract::{ConversationLog, LogFailure};
use ofx_session::{
    ChildSessions, PublicationScheduler, SessionError, SessionLog, SessionPreferences,
    WritableSession,
};

use super::SessionRoute;

pub(super) struct SessionChildren {
    sessions: ChildSessions,
    parent: Weak<Mutex<WritableSession>>,
    publisher: Option<PublicationScheduler>,
    route: SessionRoute,
    language: String,
}

struct ChildSession {
    session: Arc<Mutex<WritableSession>>,
    parent: Weak<Mutex<WritableSession>>,
    publisher: Option<PublicationScheduler>,
    route: SessionRoute,
}

impl SessionChildren {
    pub(super) fn new(
        sessions: ChildSessions,
        parent: Weak<Mutex<WritableSession>>,
        publisher: Option<PublicationScheduler>,
        route: SessionRoute,
        language: String,
    ) -> Self {
        Self {
            sessions,
            parent,
            publisher,
            route,
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

    fn load_registry(&self) -> Result<Option<Vec<u8>>, LogFailure> {
        self.sessions.load_registry().map_err(failure)
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
            provider: self.route.provider.clone(),
            model: settings.model.clone(),
            effort: settings.effort.clone(),
            fast_mode: settings.fast_mode,
            ultrafast_mode: false,
        };
        let session = self
            .sessions
            .start(child_id, preferences, &self.language)
            .map_err(failure)?;
        Ok(Arc::new(ChildSession {
            session: Arc::new(Mutex::new(session)),
            parent: Weak::clone(&self.parent),
            publisher: self.publisher.clone(),
            route: self.route.clone(),
        }))
    }

    fn resume_child(&self, child_id: &str) -> Result<ResumedChild, LogFailure> {
        let mut session = self.sessions.resume(child_id).map_err(failure)?;
        let history = session.restored_history().map_err(failure)?;
        let preferences = &session.metadata().preferences;
        let settings = ChildSettings {
            model: preferences.model.clone(),
            effort: preferences.effort.clone(),
            fast_mode: preferences.fast_mode,
        };
        Ok(ResumedChild {
            record: Arc::new(ChildSession {
                session: Arc::new(Mutex::new(session)),
                parent: Weak::clone(&self.parent),
                publisher: self.publisher.clone(),
                route: self.route.clone(),
            }),
            settings,
            history,
        })
    }

    fn reply_for_work(&self, child_id: &str, work_id: &str) -> Result<Option<String>, LogFailure> {
        self.sessions
            .reply_for_work(child_id, work_id)
            .map_err(failure)
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
        Box::new(
            SessionLog::new(
                Arc::clone(&self.session),
                self.route.provider.clone(),
                self.route.credential,
            )
            .accounting_in(Weak::clone(&self.parent))
            .publishing_with(self.publisher.clone()),
        )
    }
}

fn failure(error: SessionError) -> LogFailure {
    LogFailure {
        code: error.to_string(),
    }
}
