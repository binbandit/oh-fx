use ofx_agent::Agent;
use ofx_contract::{
    HistoryEntry, Notice, NoticeTone, ResumeRefusal, SessionCursor, SessionPage, SessionRow,
    SessionScope,
};
use ofx_session::{ListScope, ResumeContinuation, SessionError, SessionSummary};

use super::persistence::{Persistence, SESSION_TOPIC};
use super::{LiveSession, RestoredPreferences, ResumedSession};

pub(crate) struct Switched {
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) preferences: RestoredPreferences,
    pub(crate) notice: Option<Notice>,
}

pub(crate) struct Refused {
    pub(crate) refusal: ResumeRefusal,
    pub(crate) notice: Option<Notice>,
}

impl Persistence {
    pub(crate) fn page(
        &mut self,
        scope: SessionScope,
        after: Option<SessionCursor>,
        limit: usize,
    ) -> Result<SessionPage, SessionError> {
        let continuation = after.as_ref().map(|cursor| ResumeContinuation {
            updated_at_ms: cursor.updated_at_ms,
            id: cursor.id.clone(),
        });
        let catalog = match self.catalog.take() {
            Some(catalog) if after.is_some() => catalog,
            _ => self.store.catalog()?,
        };
        let listed = catalog.page(
            match scope {
                SessionScope::CurrentWorkspace => ListScope::CurrentWorkspace,
                SessionScope::AllWorkspaces => ListScope::AllWorkspaces,
            },
            self.live.as_ref().map(LiveSession::id),
            continuation.as_ref(),
            limit,
        );
        self.catalog = Some(catalog);
        Ok(SessionPage {
            scope,
            after,
            rows: listed.summaries.into_iter().map(row).collect(),
            has_more: listed.has_more,
        })
    }

    pub(crate) fn resume_selected(
        &mut self,
        id: &str,
        agent: &mut Agent,
    ) -> Result<Switched, Refused> {
        let mut session = self.store.open_without_waiting(id).map_err(refused)?;
        if session.metadata().preferences.provider != self.provider {
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
        let resumed = ResumedSession::load(session).map_err(refused)?;
        let history = resumed.transcript().map_err(refused)?;
        let preferences = self.overrides.restore(resumed.preferences());
        self.adopt_preferences(resumed.preferences());
        self.close(agent);
        agent.clear_history();
        let live = LiveSession::resume(resumed, self.provider.clone(), agent);
        live.attach(agent);
        let notice = self.remember(live.id());
        self.live = Some(live);
        Ok(Switched {
            history,
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

fn row(summary: SessionSummary) -> SessionRow {
    SessionRow {
        id: summary.id,
        title: summary.title,
        workspace_root: summary.workspace_root,
        updated_at_ms: summary.updated_at_ms,
        turns: summary.history_len,
    }
}
