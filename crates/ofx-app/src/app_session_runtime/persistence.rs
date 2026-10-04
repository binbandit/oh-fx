use ofx_agent::{Agent, TurnFailure, TurnReport};
use ofx_contract::{Notice, NoticeTone, ReasoningEffort, TurnOutcome};
use ofx_session::{SessionCatalog, SessionError, SessionPreferences, SessionStore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    LaunchOverrides, LiveSession, RenameError, ResumedSession, SessionRoute, SessionTitle,
    validate_session_title,
};
use crate::app_bootstrap_runtime::AgentSetup;

pub(super) const SESSION_TOPIC: &str = "session";

pub(crate) struct Resumption {
    pub(crate) session: ResumedSession,
    pub(crate) remember: bool,
}

pub(crate) struct Persistence {
    pub(super) store: SessionStore,
    pub(super) route: SessionRoute,
    preferences: SessionPreferences,
    pub(super) live: Option<LiveSession>,
    pub(super) overrides: LaunchOverrides,
    pub(super) catalog: Option<SessionCatalog>,
    resumption: Option<Resumption>,
    remember_fresh: bool,
    degraded: bool,
    title_task: Option<JoinHandle<()>>,
}

impl Persistence {
    pub(crate) fn new(
        store: SessionStore,
        route: SessionRoute,
        preferences: SessionPreferences,
        overrides: LaunchOverrides,
        resumption: Option<Resumption>,
    ) -> Self {
        Self {
            store,
            route,
            preferences,
            live: None,
            overrides,
            catalog: None,
            resumption,
            remember_fresh: false,
            degraded: false,
            title_task: None,
        }
    }

    pub(crate) fn active_id(&self) -> Option<&str> {
        self.live.as_ref().map(LiveSession::id)
    }

    pub(crate) fn resumed_title(&self) -> Option<String> {
        self.resumption
            .as_ref()
            .and_then(|resumption| resumption.session.display_title())
            .map(str::to_owned)
    }

    pub(crate) fn open(&mut self, agent: &mut Agent) -> Option<Notice> {
        let Some(Resumption { session, remember }) = self.resumption.take() else {
            return self.begin_fresh(agent);
        };
        let live = LiveSession::resume(session, self.route.clone(), agent);
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
        match LiveSession::start(&self.store, self.preferences.clone(), self.route.clone()) {
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

    pub(crate) fn observe_prompt(&self, prompt: &str) {
        if let Some(live) = &self.live {
            live.observe_prompt(prompt);
        }
    }

    pub(crate) fn select_model(
        &mut self,
        model: &str,
        effort: Option<&ReasoningEffort>,
        fast_mode: bool,
    ) -> Option<Notice> {
        model.clone_into(&mut self.preferences.model);
        if let Some(effort) = effort {
            effort.clone_into(&mut self.preferences.effort);
        }
        self.preferences.fast_mode = fast_mode;
        let error = self
            .live
            .as_ref()?
            .session()
            .select_model(model, effort, fast_mode)
            .err()?;
        if std::mem::replace(&mut self.degraded, true) {
            return None;
        }
        Some(non_durable("session persistence degraded", error))
    }

    pub(super) fn adopt_preferences(&mut self, saved: &SessionPreferences) {
        saved.clone_into(&mut self.preferences);
    }

    pub(crate) fn start_title_generation(
        &mut self,
        setup: &AgentSetup,
        prompt: &str,
        untitled: bool,
        title: &SessionTitle,
    ) {
        let running = self
            .title_task
            .as_ref()
            .is_some_and(|task| !task.is_finished());
        let generation = self
            .live
            .as_ref()
            .and_then(|live| live.title_generation(setup, prompt, untitled, running));
        if let Some(generation) = generation {
            let title = title.clone();
            self.title_task = Some(tokio::spawn(async move {
                if let Some(generated) = generation.run(&CancellationToken::new()).await {
                    title.set(Some(&generated));
                }
            }));
        }
    }

    pub(crate) fn rename(
        &mut self,
        raw: &str,
        cached: &SessionTitle,
    ) -> Result<String, RenameError> {
        let title = validate_session_title(raw)?;
        let live = self.live.as_ref().ok_or(RenameError::NoActiveSession)?;
        cached.set(Some(title));
        live.rename(title).map_err(RenameError::NotSaved)?;
        Ok(title.to_owned())
    }

    pub(crate) fn close(&mut self, agent: &mut Agent) {
        if let Some(task) = self.title_task.take() {
            task.abort();
        }
        agent.detach_session();
        if let Some(live) = self.live.take()
            && !live.titled()
        {
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
