use ofx_agent::Agent;
use ofx_app::{
    AgentSetup, LiveSession, Profile, ResumedSession, TitleGeneration, configured_preferences,
    session_route,
};
use ofx_session::{SessionDisposal, SessionError, SessionStore, SessionV2, SessionsV2};

pub(crate) enum SavedAsk {
    V1 {
        store: SessionStore,
        live: LiveSession,
        resumed: bool,
    },
    V2 {
        session: SessionV2,
        store: SessionsV2,
    },
}

impl SavedAsk {
    pub(crate) fn resume(
        store: SessionStore,
        resumed: ResumedSession,
        setup: &AgentSetup,
        agent: &mut Agent,
    ) -> Result<Self, SessionError> {
        let rebound_from = resumed.rebound_from().map(str::to_owned);
        let live = LiveSession::resume(resumed, session_route(setup)?, agent);
        live.attach(agent);
        if let Some(saved) = rebound_from {
            let _ = live.rebind_provider(setup.configured_model());
            crate::cli_ask::warn(&live.rebind_notice(&saved));
        }
        setup.bind_children(live.ask_children(&store));
        Ok(Self::V1 {
            store,
            live,
            resumed: true,
        })
    }

    pub(crate) fn start(
        store: SessionStore,
        profile: &Profile,
        setup: &AgentSetup,
        agent: &mut Agent,
    ) -> Result<Self, SessionError> {
        let route = session_route(setup)?;
        let preferences = configured_preferences(profile, setup, route.provider().clone());
        let live = LiveSession::start(&store, preferences, route)?;
        live.attach(agent);
        setup.bind_children(live.ask_children(&store));
        Ok(Self::V1 {
            store,
            live,
            resumed: false,
        })
    }

    pub(crate) fn start_v2(
        store: SessionsV2,
        profile: &Profile,
        setup: &AgentSetup,
        agent: &mut Agent,
        prompt: &str,
    ) -> Result<Self, SessionError> {
        let route = session_route(setup)?;
        let preferences = configured_preferences(profile, setup, route.provider().clone());
        let session = store.start_ask(&preferences, prompt)?;
        agent.attach_session(session.id().to_owned(), session.log());
        Ok(Self::V2 { session, store })
    }

    pub(crate) fn observe_prompt(&self, prompt: &str) {
        if let Self::V1 { live, .. } = self {
            live.observe_prompt(prompt);
        }
    }

    pub(crate) fn title_generation(
        &self,
        setup: &AgentSetup,
        prompt: &str,
        agent: &Agent,
    ) -> Option<TitleGeneration> {
        match self {
            Self::V1 {
                live,
                resumed: false,
                ..
            } => live.title_generation(setup, prompt, agent.history_turns() == 0, false),
            Self::V1 { .. } | Self::V2 { .. } => None,
        }
    }

    pub(crate) fn keeps_recovery(&self) -> bool {
        matches!(self, Self::V1 { .. })
    }

    pub(crate) fn finish_publications(&self) {
        if let Self::V1 { live, .. } = self {
            live.finish_publications();
        }
    }

    pub(crate) fn close(self, discard_untouched: bool) -> String {
        match self {
            Self::V1 {
                store,
                live,
                resumed,
            } => {
                let id = live.id().to_owned();
                if !discard_untouched || resumed {
                    return id;
                }
                match live.discard_if_pristine(&store) {
                    SessionDisposal::Discarded => String::new(),
                    SessionDisposal::Retained | SessionDisposal::Indeterminate => id,
                }
            }
            Self::V2 { session, store } => {
                let id = session.close();
                drop(store);
                id
            }
        }
    }
}
