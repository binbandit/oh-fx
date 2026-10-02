use std::sync::{Arc, Mutex, PoisonError};

use ofx_agent::Agent;
use ofx_app::{AgentSetup, Profile};
use ofx_config::SelectionError;
use ofx_contract::RestoredHistory;
use ofx_session::{
    ResumeTarget, SavedProvider, SessionDisposal, SessionError, SessionLog, SessionPreferences,
    SessionStore, WritableSession,
};

pub(crate) enum ResumeFailure {
    Session(SessionError),
    Selection(SelectionError),
}

impl From<SessionError> for ResumeFailure {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

pub(crate) struct Resumed {
    store: SessionStore,
    session: WritableSession,
    history: RestoredHistory,
}

impl Resumed {
    pub(crate) fn open(
        profile: &mut Profile,
        target: &ResumeTarget,
    ) -> Result<Self, ResumeFailure> {
        let store = open_store(profile)?;
        let mut session = store.resume_target(target)?;
        let preferences = &session.metadata().preferences;
        profile
            .resume_selection(
                preferences.provider.id(),
                preferences.provider.binding(),
                &preferences.model,
            )
            .map_err(ResumeFailure::Selection)?;
        let history = session.restored_history()?;
        Ok(Self {
            store,
            session,
            history,
        })
    }

    pub(crate) fn preferences(&self) -> &SessionPreferences {
        &self.session.metadata().preferences
    }
}

pub(crate) struct SavedAsk {
    store: SessionStore,
    session: Arc<Mutex<WritableSession>>,
    provider: SavedProvider,
    id: String,
    resumed: bool,
}

impl SavedAsk {
    pub(crate) fn resume(
        resumed: Resumed,
        setup: &AgentSetup,
        agent: &mut Agent,
    ) -> Result<Self, SessionError> {
        let provider = running_provider(setup)?;
        agent.restore(resumed.history);
        Ok(Self::new(resumed.store, resumed.session, provider, true))
    }

    pub(crate) fn start(
        store: SessionStore,
        profile: &Profile,
        setup: &AgentSetup,
    ) -> Result<Self, SessionError> {
        let provider = running_provider(setup)?;
        let settings = profile.settings();
        let session = store.start(SessionPreferences {
            provider: provider.clone(),
            model: setup.configured_model().to_owned(),
            effort: settings.reasoning_effort(),
            fast_mode: settings.fast_mode_for(&setup.provider(), setup.configured_model()),
        })?;
        Ok(Self::new(store, session, provider, false))
    }

    fn new(
        store: SessionStore,
        session: WritableSession,
        provider: SavedProvider,
        resumed: bool,
    ) -> Self {
        Self {
            id: session.id().to_owned(),
            store,
            session: Arc::new(Mutex::new(session)),
            provider,
            resumed,
        }
    }

    pub(crate) fn attach(&self, agent: Agent) -> Agent {
        agent
            .with_session_id(self.id.clone())
            .with_conversation_log(Box::new(SessionLog::new(
                Arc::clone(&self.session),
                self.provider.clone(),
            )))
    }

    pub(crate) fn close(self, discard_untouched: bool) -> String {
        if !discard_untouched || self.resumed {
            return self.id;
        }
        let Ok(session) = Arc::try_unwrap(self.session) else {
            return self.id;
        };
        let session = session.into_inner().unwrap_or_else(PoisonError::into_inner);
        match self.store.discard_pristine(session) {
            SessionDisposal::Discarded => String::new(),
            SessionDisposal::Retained | SessionDisposal::Indeterminate => self.id,
        }
    }
}

pub(crate) fn open_store(profile: &Profile) -> Result<SessionStore, SessionError> {
    let data = profile
        .data_dir()
        .ok_or(SessionError::SessionStoreUnavailable)?;
    let workspace = profile
        .workspace_root()
        .to_str()
        .ok_or(SessionError::InvalidWorkspaceRoot)?;
    SessionStore::open(data, workspace)
}

fn running_provider(setup: &AgentSetup) -> Result<SavedProvider, SessionError> {
    SavedProvider::new(setup.provider(), setup.provider_binding())
        .ok_or(SessionError::InvalidDurableField)
}
