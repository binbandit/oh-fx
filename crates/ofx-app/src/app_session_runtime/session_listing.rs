use std::mem;
use std::time::Duration;

use ofx_contract::{SessionCursor, SessionPage, SessionRow, SessionScope};
use ofx_session::{
    ListScope, ResumeContinuation, SessionCatalog, SessionError, SessionSource, SessionStore,
    SessionSummary,
};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const FRESH_FOR: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageRequest {
    pub(crate) scope: SessionScope,
    pub(crate) after: Option<SessionCursor>,
    pub(crate) limit: usize,
}

pub(crate) enum Listed {
    Ready(SessionPage),
    Waiting,
}

pub(crate) struct Answer {
    pub(crate) request: PageRequest,
    pub(crate) page: Result<SessionPage, SessionError>,
}

struct Held {
    catalog: SessionCatalog,
    active_id: Option<String>,
    listed_at: Instant,
}

impl Held {
    fn serves(&self, active_id: Option<&str>, request: &PageRequest, now: Instant) -> bool {
        self.active_id.as_deref() == active_id
            && (request.after.is_some()
                || now.saturating_duration_since(self.listed_at) <= FRESH_FOR)
    }
}

struct Scan {
    active_id: Option<String>,
    task: JoinHandle<Result<SessionCatalog, SessionError>>,
}

#[derive(Default)]
pub(crate) struct SessionListing {
    held: Option<Held>,
    scan: Option<Scan>,
    waiting: Vec<PageRequest>,
}

impl SessionListing {
    pub(crate) fn request(
        &mut self,
        store: &SessionStore,
        active_id: Option<&str>,
        request: PageRequest,
        now: Instant,
    ) -> Result<Listed, SessionError> {
        if let Some(held) = self
            .held
            .as_ref()
            .filter(|held| held.serves(active_id, &request, now))
        {
            return Ok(Listed::Ready(page(&held.catalog, active_id, &request)));
        }
        self.start(store, active_id)?;
        self.waiting.push(request);
        Ok(Listed::Waiting)
    }

    pub(crate) fn preload(&mut self, store: &SessionStore, active_id: Option<&str>, now: Instant) {
        let fresh = self.held.as_ref().is_some_and(|held| {
            held.serves(
                active_id,
                &PageRequest {
                    scope: SessionScope::CurrentWorkspace,
                    after: None,
                    limit: 0,
                },
                now,
            )
        });
        if !fresh && store.has_catalog_index() {
            let _ = self.start(store, active_id);
        }
    }

    pub(crate) async fn scanned(&mut self) -> Result<SessionCatalog, SessionError> {
        let Some(scan) = &mut self.scan else {
            return std::future::pending().await;
        };
        (&mut scan.task)
            .await
            .unwrap_or(Err(SessionError::SessionStoreUnavailable))
    }

    pub(crate) fn finish(
        &mut self,
        store: &SessionStore,
        active_id: Option<&str>,
        scanned: Result<SessionCatalog, SessionError>,
        now: Instant,
    ) -> Vec<Answer> {
        let Some(scan) = self.scan.take() else {
            return Vec::new();
        };
        if scan.active_id.as_deref() != active_id {
            if !self.waiting.is_empty()
                && let Err(error) = self.start(store, active_id)
            {
                return self.fail(error);
            }
            return Vec::new();
        }
        let catalog = match scanned {
            Ok(catalog) => catalog,
            Err(error) => return self.fail(error),
        };
        let held = Held {
            catalog,
            active_id: scan.active_id,
            listed_at: now,
        };
        let answers = mem::take(&mut self.waiting)
            .into_iter()
            .map(|request| Answer {
                page: Ok(page(&held.catalog, active_id, &request)),
                request,
            })
            .collect();
        self.held = Some(held);
        answers
    }

    fn start(&mut self, store: &SessionStore, active_id: Option<&str>) -> Result<(), SessionError> {
        if self
            .scan
            .as_ref()
            .is_some_and(|scan| scan.active_id.as_deref() == active_id)
        {
            return Ok(());
        }
        let store = store.try_clone()?;
        self.scan = Some(Scan {
            active_id: active_id.map(str::to_owned),
            task: tokio::task::spawn_blocking(move || store.catalog_with_fx_sessions()),
        });
        Ok(())
    }

    fn fail(&mut self, error: SessionError) -> Vec<Answer> {
        mem::take(&mut self.waiting)
            .into_iter()
            .map(|request| Answer {
                request,
                page: Err(error),
            })
            .collect()
    }
}

fn page(catalog: &SessionCatalog, active_id: Option<&str>, request: &PageRequest) -> SessionPage {
    let continuation = request.after.as_ref().map(|cursor| ResumeContinuation {
        updated_at_ms: cursor.updated_at_ms,
        id: cursor.id.clone(),
    });
    let listed = catalog.page(
        match request.scope {
            SessionScope::CurrentWorkspace => ListScope::CurrentWorkspace,
            SessionScope::AllWorkspaces => ListScope::AllWorkspaces,
        },
        active_id,
        continuation.as_ref(),
        request.limit,
    );
    SessionPage {
        scope: request.scope,
        after: request.after.clone(),
        rows: listed.summaries.into_iter().map(row).collect(),
        has_more: listed.has_more,
    }
}

fn row(summary: SessionSummary) -> SessionRow {
    SessionRow {
        id: summary.id,
        title: summary.title,
        workspace_root: summary.workspace_root,
        updated_at_ms: summary.updated_at_ms,
        turns: summary.history_len,
        from_fx: summary.source == SessionSource::Fx,
    }
}

#[cfg(test)]
mod tests;
