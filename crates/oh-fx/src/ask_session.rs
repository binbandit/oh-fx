use ofx_agent::Agent;
use ofx_app::{
    AgentSetup, LiveSession, Profile, ResumedSession, TitleGeneration, configured_preferences,
    running_provider,
};
use ofx_session::{SessionDisposal, SessionError, SessionStore};

pub(crate) struct SavedAsk {
    store: SessionStore,
    live: LiveSession,
    resumed: bool,
}

impl SavedAsk {
    pub(crate) fn resume(
        store: SessionStore,
        resumed: ResumedSession,
        setup: &AgentSetup,
        agent: &mut Agent,
    ) -> Result<Self, SessionError> {
        let live = LiveSession::resume(resumed, running_provider(setup)?, agent);
        live.attach(agent);
        Ok(Self {
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
        let provider = running_provider(setup)?;
        let preferences = configured_preferences(profile, setup, provider.clone());
        let live = LiveSession::start(&store, preferences, provider)?;
        live.attach(agent);
        Ok(Self {
            store,
            live,
            resumed: false,
        })
    }

    pub(crate) fn observe_prompt(&self, prompt: &str) {
        self.live.observe_prompt(prompt);
    }

    pub(crate) fn title_generation(
        &self,
        setup: &AgentSetup,
        prompt: &str,
        agent: &Agent,
    ) -> Option<TitleGeneration> {
        if self.resumed {
            return None;
        }
        self.live
            .title_generation(setup, prompt, agent.history_turns() == 0, false)
    }

    pub(crate) fn close(self, discard_untouched: bool) -> String {
        let id = self.live.id().to_owned();
        if !discard_untouched || self.resumed {
            return id;
        }
        match self.live.discard_if_pristine(&self.store) {
            SessionDisposal::Discarded => String::new(),
            SessionDisposal::Retained | SessionDisposal::Indeterminate => id,
        }
    }
}
