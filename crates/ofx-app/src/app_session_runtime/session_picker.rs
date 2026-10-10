use ofx_agent::Agent;
use ofx_contract::{HistoryEntry, Notice, NoticeTone, ResumeRefusal};
use ofx_session::{SessionCatalog, SessionError};
use tokio::time::Instant;

use super::persistence::{Persistence, SESSION_TOPIC};
use super::{
    Answer, Listed, LiveSession, PageRequest, RestoredPreferences, ResumedSession, SessionListing,
};
use crate::app_bootstrap_runtime::AgentSetup;
use crate::output_contracts::sessions::session_lookup_message;

pub(crate) struct Switched {
    pub(crate) continues: bool,
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) title: Option<String>,
    pub(crate) preferences: RestoredPreferences,
    pub(crate) notices: Vec<Notice>,
}

pub(crate) struct Refused {
    pub(crate) refusal: ResumeRefusal,
    pub(crate) notice: Option<Notice>,
}

impl Persistence {
    pub(crate) fn list(
        &self,
        listing: &mut SessionListing,
        request: PageRequest,
    ) -> Result<Listed, SessionError> {
        listing.request(&self.store, self.active_id(), request, Instant::now())
    }

    pub(crate) fn preload(&self, listing: &mut SessionListing) {
        listing.preload(&self.store, self.active_id(), Instant::now());
    }

    pub(crate) fn finish_listing(
        &self,
        listing: &mut SessionListing,
        scanned: Result<SessionCatalog, SessionError>,
    ) -> Vec<Answer> {
        listing.finish(&self.store, self.active_id(), scanned, Instant::now())
    }

    pub(crate) fn resume_selected(
        &mut self,
        id: &str,
        agent: &mut Agent,
        setup: &AgentSetup,
    ) -> Result<Switched, Refused> {
        let mut session = self.store.open_without_waiting(id).map_err(refused)?;
        let saved = session.metadata().preferences.provider.clone();
        let mut notices = Vec::new();
        if saved != self.route.provider && !setup.routes(saved.id()) {
            session
                .rebind_provider(self.route.provider.clone(), &self.preferences.model)
                .map_err(refused)?;
            notices.push(Notice::new(
                NoticeTone::Warning,
                SESSION_TOPIC,
                self.route.rebind_notice(saved.id().label()),
            ));
        } else if saved != self.route.provider {
            return Err(Refused {
                refusal: ResumeRefusal::Unavailable,
                notice: Some(Notice::new(
                    NoticeTone::Warning,
                    SESSION_TOPIC,
                    format!(
                        "This session was saved with another provider. Resume it with oh-fx resume {id}."
                    ),
                )),
            });
        }
        self.store.move_here(&mut session).map_err(refused)?;
        let mut resumed = ResumedSession::for_shell(session).map_err(refused)?;
        let continues = resumed.take_continuation();
        let history = resumed.transcript(setup).map_err(refused)?;
        let title = resumed.display_title().map(str::to_owned);
        let preferences = self.overrides.restore(resumed.preferences());
        self.adopt_preferences(resumed.preferences());
        self.close(agent);
        agent.clear_history();
        let live = LiveSession::resume(resumed, self.route.clone(), agent);
        live.attach(agent);
        notices.extend(self.remember(live.id()));
        self.live = Some(live);
        Ok(Switched {
            continues,
            history,
            title,
            preferences,
            notices,
        })
    }
}

fn refused(error: SessionError) -> Refused {
    let explained = match error {
        SessionError::FxSessionOpen
        | SessionError::FxCompactionUnfinished
        | SessionError::FxSessionUnreadable => session_lookup_message(&error.to_string()),
        _ => None,
    };
    Refused {
        refusal: match error {
            SessionError::SessionBusy => ResumeRefusal::OpenElsewhere,
            _ => ResumeRefusal::Unavailable,
        },
        notice: explained.map(|message| Notice::new(NoticeTone::Warning, SESSION_TOPIC, message)),
    }
}
