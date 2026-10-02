use ofx_agent::{Agent, TurnFailure, TurnReport};
use ofx_contract::{Notice, NoticeTone, TurnOutcome};
use ofx_session::{SavedProvider, SessionError, SessionPreferences, SessionStore};

use super::{LaunchOverrides, LiveSession, ResumedSession};

pub(super) const SESSION_TOPIC: &str = "session";

pub(crate) struct Resumption {
    pub(crate) session: ResumedSession,
    pub(crate) remember: bool,
}

pub(crate) struct Persistence {
    pub(super) store: SessionStore,
    pub(super) provider: SavedProvider,
    preferences: SessionPreferences,
    pub(super) live: Option<LiveSession>,
    pub(super) overrides: LaunchOverrides,
    resumption: Option<Resumption>,
    remember_fresh: bool,
    degraded: bool,
}

impl Persistence {
    pub(crate) fn new(
        store: SessionStore,
        provider: SavedProvider,
        preferences: SessionPreferences,
        overrides: LaunchOverrides,
        resumption: Option<Resumption>,
    ) -> Self {
        Self {
            store,
            provider,
            preferences,
            live: None,
            overrides,
            resumption,
            remember_fresh: false,
            degraded: false,
        }
    }

    pub(crate) fn open(&mut self, agent: &mut Agent) -> Option<Notice> {
        let Some(Resumption { session, remember }) = self.resumption.take() else {
            return self.begin_fresh(agent);
        };
        let live = LiveSession::resume(session, self.provider.clone(), agent);
        live.attach(agent);
        let notice = if remember {
            self.remember(live.id())
        } else {
            None
        };
        self.live = Some(live);
        notice
    }

    pub(crate) fn begin_fresh(&mut self, agent: &mut Agent) -> Option<Notice> {
        self.close(agent);
        match LiveSession::start(&self.store, self.preferences.clone(), self.provider.clone()) {
            Ok(live) => {
                live.attach(agent);
                self.live = Some(live);
                self.remember_fresh = true;
                self.degraded = false;
                None
            }
            Err(error) => Some(non_durable("session creation failed", error)),
        }
    }

    pub(crate) fn begin_unless_open(&mut self, agent: &mut Agent) -> Option<Notice> {
        if self.live.is_some() {
            return None;
        }
        self.begin_fresh(agent)
    }

    pub(crate) fn finish_turn(&mut self, report: &TurnReport) -> Option<Notice> {
        let live = self.live.as_ref()?;
        if let Some(TurnFailure::Persistence(failure)) = &report.failure
            && report.outcome != TurnOutcome::Failed
        {
            let consequence = if live.session().turn_open() {
                "New messages are blocked until you reopen the session. The turn may run again."
            } else {
                "The session keeps running; this turn may be missing after a resume."
            };
            return Some(Notice::new(
                NoticeTone::Error,
                SESSION_TOPIC,
                format!(
                    "Turn completed, but oh-fx could not save it ({}). {consequence}",
                    failure.code
                ),
            ));
        }
        if !self.remember_fresh || live.session().last_seq() == 0 {
            return None;
        }
        self.remember_fresh = false;
        self.remember(live.id())
    }

    pub(crate) fn select_model(&mut self, model: &str, fast_mode: bool) -> Option<Notice> {
        model.clone_into(&mut self.preferences.model);
        self.preferences.fast_mode = fast_mode;
        let error = self
            .live
            .as_ref()?
            .session()
            .select_model(model, fast_mode)
            .err()?;
        if std::mem::replace(&mut self.degraded, true) {
            return None;
        }
        Some(non_durable("session persistence degraded", error))
    }

    pub(crate) fn close(&mut self, agent: &mut Agent) {
        agent.detach_session();
        if let Some(live) = self.live.take() {
            live.discard_if_pristine(&self.store);
        }
        self.remember_fresh = false;
    }

    pub(super) fn remember(&self, id: &str) -> Option<Notice> {
        let error = self.store.remember_session_id(id).err()?;
        Some(Notice::new(
            NoticeTone::Warning,
            SESSION_TOPIC,
            format!(
                "Session saved, but could not remember it for -c ({error}). Resume with oh-fx --resume {id}."
            ),
        ))
    }
}

fn non_durable(label: &str, error: SessionError) -> Notice {
    Notice::new(
        NoticeTone::Warning,
        SESSION_TOPIC,
        format!("{label}: {error}; continuing non-durably"),
    )
}
