use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::launch::{LaunchError, TerminalLaunch, launch_terminal};
use super::session::{Session, SessionLost};
use crate::managed_execution::MAX_LIVE_ENTRIES;
use crate::self_exe::path_for_peer_reexec;
use crate::shell_resolver::{ResolveError, configured_login_shell, resolve_terminal_shell};
use crate::terminal_contracts::{
    Action, Dimensions, ReturnCondition, ReturnOutcome, SessionFacts, ShellSpec,
    StructuredErrorCode, TerminalFailure,
};
use crate::terminal_store::{NewTerminal, StoreError, TerminalStore, now_ms};

const DEFAULT_DIMENSIONS: Dimensions = Dimensions {
    rows: 24,
    columns: 80,
};
const EXIT_GRACE: Duration = Duration::from_millis(500);
const EXIT_JOIN: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct StartRequest<'a> {
    pub owner_session_id: &'a str,
    pub cwd: &'a Path,
    pub command: &'a str,
    pub shell: &'a ShellSpec,
    pub return_when: ReturnCondition,
    pub wait_ceiling: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub session: SessionFacts,
    pub outcome: ReturnOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectResult {
    pub session: SessionFacts,
    pub shell: String,
    pub cwd: String,
    pub command: Option<String>,
}

#[derive(Clone)]
pub struct Terminals {
    registry: Arc<Registry>,
}

struct Registry {
    store: TerminalStore,
    slots: Mutex<Slots>,
}

#[derive(Default)]
struct Slots {
    sessions: Vec<Arc<Session>>,
    recycle_cursor: usize,
    exiting: bool,
}

impl Terminals {
    #[must_use]
    pub fn new(store: TerminalStore) -> Self {
        Self {
            registry: Arc::new(Registry {
                store,
                slots: Mutex::new(Slots::default()),
            }),
        }
    }

    pub async fn start(
        &self,
        request: &StartRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<Outcome, TerminalFailure> {
        let refuse = |code| TerminalFailure::new(Action::Start, code, None);
        let (shell, clean_start) =
            resolve_terminal_shell(configured_login_shell().as_deref(), request.shell).map_err(
                |error| {
                    refuse(match error {
                        ResolveError::MissingLoginShell => StructuredErrorCode::ShellUnavailable,
                        ResolveError::RelativeShellPath | ResolveError::UnsupportedShell => {
                            StructuredErrorCode::InvalidRequest
                        }
                    })
                },
            )?;
        let durable = self
            .registry
            .store
            .create(
                &NewTerminal {
                    owner_session_id: request.owner_session_id,
                    shell: &shell.to_string_lossy(),
                    cwd: &request.cwd.to_string_lossy(),
                    command: request.command,
                    timeout_ms: None,
                    dimensions: DEFAULT_DIMENSIONS,
                },
                now_ms(),
            )
            .map_err(|error| {
                refuse(match error {
                    StoreError::CapacityExceeded => StructuredErrorCode::CapacityExceeded,
                    _ => StructuredErrorCode::AuthorityDenied,
                })
            })?;
        let session = Arc::new(Session::new(durable));
        if !self.registry.reserve(&session) {
            session.abandon_unreleased();
            return Err(refuse(if self.registry.exiting() {
                StructuredErrorCode::Cancelled
            } else {
                StructuredErrorCode::CapacityExceeded
            }));
        }
        if let Err(code) = launch(&session, &shell, clean_start, request) {
            self.registry.remove(&session);
            session.abandon_unreleased();
            session.finalize_backend().await;
            return Err(refuse(code));
        }
        if self.registry.exiting() {
            session.hang_up_for_exit();
            session.kill_for_exit();
        }
        match session
            .wait_for(request.return_when, request.wait_ceiling, cancel)
            .await
        {
            Err(SessionLost) => {
                let code = session.start_failure();
                session.finalize_backend().await;
                Err(TerminalFailure::new(Action::Start, code, Some(&session.id)))
            }
            Ok(outcome) => {
                if outcome.is_terminal() {
                    session.finalize_backend().await;
                }
                Ok(Outcome {
                    session: session.facts(),
                    outcome,
                })
            }
        }
    }

    pub async fn wait(
        &self,
        owner: &str,
        session_id: &str,
        condition: ReturnCondition,
        ceiling: Duration,
        cancel: &CancellationToken,
    ) -> Result<Outcome, TerminalFailure> {
        let session = self.live(Action::Wait, owner, session_id)?;
        match session.wait_for(condition, ceiling, cancel).await {
            Err(SessionLost) => {
                session.finalize_backend().await;
                Err(TerminalFailure::new(
                    Action::Wait,
                    StructuredErrorCode::SessionLost,
                    Some(session_id),
                ))
            }
            Ok(outcome) => {
                if outcome.is_terminal() {
                    session.finalize_backend().await;
                }
                Ok(Outcome {
                    session: session.facts(),
                    outcome,
                })
            }
        }
    }

    pub fn inspect(&self, owner: &str, session_id: &str) -> Result<InspectResult, TerminalFailure> {
        let session = self.live(Action::Inspect, owner, session_id)?;
        Ok(InspectResult {
            session: session.facts(),
            shell: session.shell.clone(),
            cwd: session.cwd.clone(),
            command: Some(session.command.clone()),
        })
    }

    pub async fn close_owned_terminals(&self) {
        let sessions = self.registry.begin_exit();
        let hung_up = sessions
            .iter()
            .filter(|session| session.hang_up_for_exit())
            .count();
        if hung_up != 0 {
            let deadline = Instant::now() + EXIT_GRACE;
            for session in &sessions {
                session.settle_before(deadline).await;
            }
            for session in &sessions {
                session.kill_for_exit();
            }
        }
        let deadline = Instant::now() + EXIT_JOIN;
        for session in &sessions {
            session.finalize_backend_before(deadline).await;
        }
    }

    fn live(
        &self,
        action: Action,
        owner: &str,
        session_id: &str,
    ) -> Result<Arc<Session>, TerminalFailure> {
        let failure = |code| TerminalFailure::new(action, code, Some(session_id));
        let session = self
            .registry
            .find(session_id)
            .ok_or_else(|| failure(StructuredErrorCode::SessionNotFound))?;
        if session.owner != owner {
            return Err(failure(StructuredErrorCode::AuthorityDenied));
        }
        Ok(session)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        for session in &self.slots().sessions {
            session.shutdown();
        }
    }
}

impl Registry {
    fn slots(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reserve(&self, session: &Arc<Session>) -> bool {
        let mut slots = self.slots();
        if slots.exiting {
            return false;
        }
        if slots.sessions.len() < MAX_LIVE_ENTRIES {
            slots.sessions.push(Arc::clone(session));
            return true;
        }
        let cursor = slots.recycle_cursor;
        let Some(index) = (0..MAX_LIVE_ENTRIES)
            .map(|offset| (cursor + offset) % MAX_LIVE_ENTRIES)
            .find(|&index| {
                let candidate = &slots.sessions[index];
                Arc::strong_count(candidate) == 1 && candidate.is_recyclable()
            })
        else {
            return false;
        };
        slots.sessions[index] = Arc::clone(session);
        slots.recycle_cursor = (index + 1) % MAX_LIVE_ENTRIES;
        true
    }

    fn find(&self, session_id: &str) -> Option<Arc<Session>> {
        self.slots()
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
    }

    fn remove(&self, session: &Arc<Session>) {
        self.slots()
            .sessions
            .retain(|candidate| !Arc::ptr_eq(candidate, session));
    }

    fn exiting(&self) -> bool {
        self.slots().exiting
    }

    fn begin_exit(&self) -> Vec<Arc<Session>> {
        let mut slots = self.slots();
        slots.exiting = true;
        slots.sessions.clone()
    }
}

fn launch(
    session: &Arc<Session>,
    shell: &Path,
    clean_start: bool,
    request: &StartRequest<'_>,
) -> Result<(), StructuredErrorCode> {
    let executable = path_for_peer_reexec().map_err(|_| StructuredErrorCode::StartupFailed)?;
    let launched = launch_terminal(&TerminalLaunch {
        executable: &executable,
        shell,
        clean_start,
        cwd: request.cwd,
        command: request.command,
        dimensions: DEFAULT_DIMENSIONS,
    })
    .map_err(launch_failure_code)?;
    session.attach(launched)
}

fn launch_failure_code(error: LaunchError) -> StructuredErrorCode {
    match error {
        LaunchError::Resolve(ResolveError::MissingLoginShell) => {
            StructuredErrorCode::ShellUnavailable
        }
        LaunchError::PtyUnavailable
        | LaunchError::ResizeFailed
        | LaunchError::LauncherNotPrepared => StructuredErrorCode::PtyUnavailable,
        LaunchError::Resolve(ResolveError::RelativeShellPath | ResolveError::UnsupportedShell)
        | LaunchError::LauncherConfigTooLarge => StructuredErrorCode::InvalidRequest,
        LaunchError::Failed(_) => StructuredErrorCode::StartupFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unlaunched(registry: &Registry) -> Arc<Session> {
        let durable = registry
            .store
            .create(
                &NewTerminal {
                    owner_session_id: "owner-session",
                    shell: "/bin/bash",
                    cwd: "/",
                    command: "true",
                    timeout_ms: None,
                    dimensions: DEFAULT_DIMENSIONS,
                },
                0,
            )
            .unwrap();
        Arc::new(Session::new(durable))
    }

    fn end(registry: &Registry, id: &str) {
        registry.find(id).unwrap().abandon_unreleased();
    }

    #[test]
    fn reservations_stop_at_capacity_and_recycle_ended_terminals_in_turn() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Registry {
            store: TerminalStore::new(directory.path()).unwrap(),
            slots: Mutex::new(Slots::default()),
        };
        let ids: Vec<String> = (0..MAX_LIVE_ENTRIES)
            .map(|_| {
                let session = unlaunched(&registry);
                assert!(registry.reserve(&session));
                session.id.clone()
            })
            .collect();
        let refused = unlaunched(&registry);
        assert!(!registry.reserve(&refused));
        refused.abandon_unreleased();
        assert_eq!(
            registry.store.open("owner-session", &refused.id).err(),
            Some(StoreError::RecordNotFound)
        );
        let first = unlaunched(&registry);
        assert!(!registry.reserve(&first));
        let held = registry.find(&ids[3]).unwrap();
        held.abandon_unreleased();
        assert!(!registry.reserve(&first));
        drop(held);
        assert!(registry.reserve(&first));
        assert!(registry.find(&ids[3]).is_none());
        assert!(registry.find(&first.id).is_some());
        end(&registry, &ids[1]);
        end(&registry, &ids[5]);
        let second = unlaunched(&registry);
        assert!(registry.reserve(&second));
        assert!(registry.find(&ids[5]).is_none());
        assert!(registry.find(&ids[1]).is_some());
        assert_eq!(registry.begin_exit().len(), MAX_LIVE_ENTRIES);
        assert!(!registry.reserve(&unlaunched(&registry)));
    }

    #[test]
    fn launch_failures_keep_upstreams_codes() {
        for (error, code) in [
            (
                LaunchError::Resolve(ResolveError::MissingLoginShell),
                StructuredErrorCode::ShellUnavailable,
            ),
            (
                LaunchError::PtyUnavailable,
                StructuredErrorCode::PtyUnavailable,
            ),
            (
                LaunchError::ResizeFailed,
                StructuredErrorCode::PtyUnavailable,
            ),
            (
                LaunchError::LauncherNotPrepared,
                StructuredErrorCode::PtyUnavailable,
            ),
            (
                LaunchError::Resolve(ResolveError::UnsupportedShell),
                StructuredErrorCode::InvalidRequest,
            ),
            (
                LaunchError::LauncherConfigTooLarge,
                StructuredErrorCode::InvalidRequest,
            ),
            (
                LaunchError::Failed("FileNotFound"),
                StructuredErrorCode::StartupFailed,
            ),
        ] {
            assert_eq!(launch_failure_code(error), code, "{error:?}");
        }
    }
}
