use ofx_agent::Agent;
use ofx_contract::{HistoryEntry, Notice, NoticeTone, ResumeRefusal};
use ofx_session::{PendingRecovery, SessionCatalog, SessionError};
use tokio::time::Instant;

use super::persistence::{Persistence, SESSION_TOPIC};
use super::{
    Answer, Listed, LiveSession, PageRequest, RestoredPreferences, ResumedSession, SessionListing,
};
use crate::app_bootstrap_runtime::AgentSetup;

pub(crate) struct Switched {
    pub(crate) pending: Option<PendingRecovery>,
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) title: Option<String>,
    pub(crate) preferences: RestoredPreferences,
    pub(crate) notice: Option<Notice>,
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
        if session.metadata().preferences.provider != self.route.provider {
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
        let pending = resumed.take_pending_recovery();
        let history = resumed.transcript(setup).map_err(refused)?;
        let title = resumed.display_title().map(str::to_owned);
        let preferences = self.overrides.restore(resumed.preferences());
        self.adopt_preferences(resumed.preferences());
        self.close(agent);
        agent.clear_history();
        let live = LiveSession::resume(resumed, self.route.clone(), agent);
        live.attach(agent);
        let notice = self.remember(live.id());
        self.live = Some(live);
        Ok(Switched {
            pending,
            history,
            title,
            preferences,
            notice,
        })
    }
}

fn refused(error: SessionError) -> Refused {
    Refused {
        refusal: match error {
            SessionError::SessionBusy => ResumeRefusal::OpenElsewhere,
            _ => ResumeRefusal::Unavailable,
        },
        notice: None,
    }
}
