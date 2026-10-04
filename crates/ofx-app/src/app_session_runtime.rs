mod launch_overrides;
mod persistence;
mod resume_transcript;
mod session_picker;
mod session_titles;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_agent::Agent;
use ofx_config::SelectionError;
use ofx_contract::{HistoryEntry, RestoredHistory};
use ofx_session::{
    ResumeTarget, SavedProvider, SessionDisposal, SessionError, SessionLog, SessionPreferences,
    SessionStore, TitleGate, WritableSession, prompt_excerpt,
};

use crate::app_bootstrap_runtime::{AgentSetup, Profile};

pub(crate) use launch_overrides::{LaunchOverrides, RestoredPreferences};
pub(crate) use persistence::{Persistence, Resumption};
pub use session_titles::TitleGeneration;
pub(crate) use session_titles::{RenameError, SessionTitle, validate_session_title};

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
    title_present: bool,
}

impl ResumedSession {
    pub fn open(
        store: &SessionStore,
        profile: &mut Profile,
        target: &ResumeTarget,
    ) -> Result<Self, ResumeFailure> {
        let session = store.resume_target(target)?;
        let preferences = &session.metadata().preferences;
        profile
            .resume_selection(
                preferences.provider.id(),
                preferences.provider.binding(),
                &preferences.model,
            )
            .map_err(ResumeFailure::Selection)?;
        Ok(Self::load(session)?)
    }

    fn load(mut session: WritableSession) -> Result<Self, SessionError> {
        let title = session.display_title();
        let history = session.restored_history()?;
        let title_present = session.title().is_some()
            || history.checkpoint.is_some()
            || !history.turn_starts.is_empty();
        Ok(Self {
            session,
            history,
            title,
            title_present,
        })
    }

    pub fn preferences(&self) -> &SessionPreferences {
        &self.session.metadata().preferences
    }

    pub(crate) fn transcript(&self, setup: &AgentSetup) -> Result<Vec<HistoryEntry>, SessionError> {
        resume_transcript::transcript(&self.session, &self.title, &|tool_name, arguments| {
            setup.describe_saved_call(tool_name, arguments)
        })
    }

    pub(crate) fn display_title(&self) -> Option<&str> {
        self.title_present.then_some(self.title.as_str())
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

    pub fn observe_prompt(&self, prompt: &str) {
        self.session().observe_prompt(prompt);
    }

    fn session(&self) -> MutexGuard<'_, WritableSession> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn title_generation(
        &self,
        setup: &AgentSetup,
        prompt: &str,
        untitled: bool,
        task_running: bool,
    ) -> Option<TitleGeneration> {
        let excerpt = prompt_excerpt(prompt)?;
        let gate = TitleGate {
            setting_enabled: setup.session_titles_enabled(),
            title_model: setup.title_model(),
            session_untitled: untitled,
            task_running,
        };
        if !gate.should_generate() {
            return None;
        }
        Some(TitleGeneration {
            provider: setup.model_provider(),
            model: setup.title_model()?,
            session_id: self.id.clone(),
            excerpt: excerpt.to_owned(),
            session: Arc::downgrade(&self.session),
        })
    }

    pub(crate) fn rename(&self, title: &str) -> Result<(), SessionError> {
        self.session().rename(title)
    }

    pub(crate) fn titled(&self) -> bool {
        self.session().title().is_some()
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
