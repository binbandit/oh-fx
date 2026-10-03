mod persistence;
mod resume_transcript;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_agent::Agent;
use ofx_config::SelectionError;
use ofx_contract::{HistoryEntry, RestoredHistory};
use ofx_session::{
    ResumeTarget, SavedProvider, SessionDisposal, SessionError, SessionLog, SessionPreferences,
    SessionStore, WritableSession,
};

use crate::app_bootstrap_runtime::{AgentSetup, Profile};

pub(crate) use persistence::{Persistence, Resumption};

#[derive(Debug)]
pub enum ResumeFailure {
    Session(SessionError),
    Selection(SelectionError),
}

impl From<SessionError> for ResumeFailure {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

pub struct ResumedSession {
    session: WritableSession,
    history: RestoredHistory,
    title: String,
}

impl ResumedSession {
    pub fn open(
        store: &SessionStore,
        profile: &mut Profile,
        target: &ResumeTarget,
    ) -> Result<Self, ResumeFailure> {
        let mut session = store.resume_target(target)?;
        let preferences = &session.metadata().preferences;
        profile
            .resume_selection(
                preferences.provider.id(),
                preferences.provider.binding(),
                &preferences.model,
            )
            .map_err(ResumeFailure::Selection)?;
        let title = session.display_title();
        let history = session.restored_history()?;
        Ok(Self {
            session,
            history,
            title,
        })
    }

    pub fn preferences(&self) -> &SessionPreferences {
        &self.session.metadata().preferences
    }

    pub(crate) fn transcript(&self) -> Result<Vec<HistoryEntry>, SessionError> {
        resume_transcript::transcript(&self.session, &self.title)
    }
}

pub struct LiveSession {
    session: Arc<Mutex<WritableSession>>,
    provider: SavedProvider,
    id: String,
}

impl LiveSession {
    pub fn start(
        store: &SessionStore,
        preferences: SessionPreferences,
        provider: SavedProvider,
    ) -> Result<Self, SessionError> {
        Ok(Self::new(store.start(preferences)?, provider))
    }

    pub fn resume(resumed: ResumedSession, provider: SavedProvider, agent: &mut Agent) -> Self {
        agent.restore(resumed.history);
        Self::new(resumed.session, provider)
    }

    fn new(session: WritableSession, provider: SavedProvider) -> Self {
        Self {
            id: session.id().to_owned(),
            session: Arc::new(Mutex::new(session)),
            provider,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn attach(&self, agent: &mut Agent) {
        agent.attach_session(
            self.id.clone(),
            Box::new(SessionLog::new(
                Arc::clone(&self.session),
                self.provider.clone(),
            )),
        );
    }

    fn session(&self) -> MutexGuard<'_, WritableSession> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn discard_if_pristine(self, store: &SessionStore) -> SessionDisposal {
        match Arc::try_unwrap(self.session) {
            Ok(session) => {
                store.discard_pristine(session.into_inner().unwrap_or_else(PoisonError::into_inner))
            }
            Err(_) => SessionDisposal::Retained,
        }
    }
}

pub fn open_store(profile: &Profile) -> Result<SessionStore, SessionError> {
    let data = profile
        .data_dir()
        .ok_or(SessionError::SessionStoreUnavailable)?;
    let workspace = profile
        .workspace_root()
        .to_str()
        .ok_or(SessionError::InvalidWorkspaceRoot)?;
    SessionStore::open(data, workspace)
}

pub fn running_provider(setup: &AgentSetup) -> Result<SavedProvider, SessionError> {
    SavedProvider::new(setup.provider(), setup.provider_binding())
        .ok_or(SessionError::InvalidDurableField)
}

pub fn configured_preferences(
    profile: &Profile,
    setup: &AgentSetup,
    provider: SavedProvider,
) -> SessionPreferences {
    let settings = profile.settings();
    SessionPreferences {
        provider,
        model: setup.configured_model().to_owned(),
        effort: settings.reasoning_effort(),
        fast_mode: settings.fast_mode_for(&setup.provider(), setup.configured_model()),
    }
}
