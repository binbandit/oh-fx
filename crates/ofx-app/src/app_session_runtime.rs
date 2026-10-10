mod child_sessions;
mod launch_overrides;
mod persistence;
mod resume_transcript;
mod session_listing;
mod session_picker;
mod session_titles;
mod shell_recovery;

use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_agent::{Agent, ChildStore};
use ofx_config::SelectionError;
use ofx_contract::{HistoryEntry, RecoveredTurn, RestoredHistory};
use ofx_session::{
    PendingRecovery, ResumeTarget, RouteCredential, SavedProvider, SessionDisposal, SessionError,
    SessionLog, SessionPreferences, SessionStore, TitleGate, WritableSession, prompt_excerpt,
};

use crate::app_bootstrap_runtime::{AgentSetup, Profile};

use child_sessions::SessionChildren;
pub(crate) use launch_overrides::{LaunchOverrides, RestoredPreferences};
pub(crate) use persistence::{Persistence, Resumption};
pub(crate) use session_listing::{Answer, Listed, PageRequest, SessionListing};
pub use session_titles::TitleGeneration;
pub(crate) use session_titles::{RenameError, SessionTitle, validate_session_title};
pub(crate) use shell_recovery::{NOT_CONTINUED, RECOVERY_TOPIC, SIGN_IN_TO_CONTINUE};
use shell_recovery::{ShellRecovery, shell_recovery};

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
    recovery: Option<ShellRecovery>,
    rebound_from: Option<String>,
}

impl ResumedSession {
    pub fn open(
        store: &SessionStore,
        profile: &mut Profile,
        target: &ResumeTarget,
    ) -> Result<Self, ResumeFailure> {
        let session = store.resume_target(target)?;
        let rebound_from = select(profile, &session)?;
        Ok(Self {
            rebound_from,
            ..Self::for_shell(session)?
        })
    }

    fn for_shell(mut session: WritableSession) -> Result<Self, SessionError> {
        let recovery = shell_recovery(&mut session);
        let mut resumed = Self::load(session)?;
        resumed.recovery = recovery;
        Ok(resumed)
    }

    pub(crate) fn take_continuation(&mut self) -> bool {
        self.recovery
            .as_mut()
            .is_some_and(|recovery| mem::take(&mut recovery.continues))
    }

    pub fn open_for_ask(
        store: &SessionStore,
        profile: &mut Profile,
        target: &ResumeTarget,
        continue_recovery: bool,
    ) -> Result<(Self, Option<PendingRecovery>), ResumeFailure> {
        let mut session = store.resume_target(target)?;
        let pending = if continue_recovery {
            Some(
                session
                    .take_recovery()
                    .ok_or(SessionError::NoPendingRecovery)?,
            )
        } else {
            session.settle_open_recovery()?;
            None
        };
        let rebound_from = select(profile, &session)?;
        Ok((
            Self {
                rebound_from,
                ..Self::load(session)?
            },
            pending,
        ))
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
            recovery: None,
            rebound_from: None,
        })
    }

    pub fn preferences(&self) -> &SessionPreferences {
        &self.session.metadata().preferences
    }

    pub fn rebound_from(&self) -> Option<&str> {
        self.rebound_from.as_deref()
    }

    pub(crate) fn transcript(&self, setup: &AgentSetup) -> Result<Vec<HistoryEntry>, SessionError> {
        let mut entries =
            resume_transcript::transcript(&self.session, &self.title, &|tool_name, arguments| {
                setup.describe_saved_call(tool_name, arguments)
            })?;
        if let Some(recovery) = &self.recovery {
            entries.extend(recovery.entries.iter().cloned());
        }
        Ok(entries)
    }

    pub(crate) fn display_title(&self) -> Option<&str> {
        self.title_present.then_some(self.title.as_str())
    }
}

fn select(
    profile: &mut Profile,
    session: &WritableSession,
) -> Result<Option<String>, ResumeFailure> {
    let preferences = &session.metadata().preferences;
    let provider = preferences.provider.id();
    if !profile.settings().routes(provider) {
        return Ok(Some(provider.label().to_owned()));
    }
    profile
        .resume_selection(provider, preferences.provider.binding(), &preferences.model)
        .map_err(ResumeFailure::Selection)?;
    Ok(None)
}

pub fn recovered_turn(
    pending: PendingRecovery,
    setup: &AgentSetup,
) -> Result<RecoveredTurn, SessionError> {
    if !pending.authorizes(setup.route_credential()) {
        return Err(SessionError::RecoveryCredentialAuthorityChanged);
    }
    Ok(pending.into_turn(&running_provider(setup)?, setup.model(), setup.fast_mode()))
}

pub struct LiveSession {
    session: Arc<Mutex<WritableSession>>,
    route: SessionRoute,
    id: String,
}

impl LiveSession {
    pub fn start(
        store: &SessionStore,
        preferences: SessionPreferences,
        route: SessionRoute,
    ) -> Result<Self, SessionError> {
        Ok(Self::new(store.start(preferences)?, route))
    }

    pub fn resume(resumed: ResumedSession, route: SessionRoute, agent: &mut Agent) -> Self {
        agent.restore(resumed.history);
        Self::new(resumed.session, route)
    }

    fn new(session: WritableSession, route: SessionRoute) -> Self {
        Self {
            id: session.id().to_owned(),
            session: Arc::new(Mutex::new(session)),
            route,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn children(&self, store: &SessionStore) -> Option<Arc<dyn ChildStore>> {
        let sessions = store.children(&self.id).ok()?;
        let language = self.session().metadata().conversation_language.clone();
        Some(Arc::new(SessionChildren::new(
            sessions,
            self.route.clone(),
            language,
        )))
    }

    pub fn attach(&self, agent: &mut Agent) {
        agent.attach_session(
            self.id.clone(),
            Box::new(SessionLog::new(
                Arc::clone(&self.session),
                self.route.provider.clone(),
                self.route.credential,
            )),
        );
    }

    pub fn observe_prompt(&self, prompt: &str) {
        self.session().observe_prompt(prompt);
    }

    pub fn rebind_provider(&self, model: &str) -> Result<(), SessionError> {
        self.session()
            .rebind_provider(self.route.provider.clone(), model)
    }

    pub fn rebind_notice(&self, saved: &str) -> String {
        format!(
            "This session was saved with the {saved} provider, which oh-fx cannot use yet; it continues with {}.",
            self.route.provider.id().label()
        )
    }

    pub(crate) fn continue_recovery(
        &self,
        setup: &AgentSetup,
        model: &str,
        fast_mode: bool,
    ) -> Result<RecoveredTurn, SessionError> {
        let provider = running_provider(setup)?;
        let pending = self
            .session()
            .take_authorized_recovery(setup.route_credential())?;
        Ok(pending.into_turn(&provider, model, fast_mode))
    }

    pub(crate) fn settle_open_recovery(&self, agent: &mut Agent) -> Result<(), SessionError> {
        let mut session = self.session();
        if !session.turn_open() || !session.holds_recovery() {
            return Ok(());
        }
        session.settle_open_recovery()?;
        agent.restore(session.restored_history()?);
        Ok(())
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
    let store = SessionStore::open(data, workspace)?;
    Ok(match profile.home() {
        Some(home) => store.with_fx_home(home.to_path_buf()),
        None => store,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoute {
    pub(crate) provider: SavedProvider,
    pub(crate) credential: RouteCredential,
}

impl SessionRoute {
    pub fn provider(&self) -> &SavedProvider {
        &self.provider
    }
}

pub fn session_route(setup: &AgentSetup) -> Result<SessionRoute, SessionError> {
    Ok(SessionRoute {
        provider: running_provider(setup)?,
        credential: setup.route_credential(),
    })
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
