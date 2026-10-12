use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ofx_contract::valid_session_id;
use ofx_vt::Grid;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::launch::{LaunchError, TerminalLaunch, launch_terminal};
use super::session::{Session, SessionLost};
use crate::managed_execution::MAX_LIVE_ENTRIES;
use crate::self_exe::path_for_peer_reexec;
use crate::shell_resolver::{ResolveError, configured_login_shell, resolve_terminal_shell};
use crate::terminal_contracts::{
    Action, ClosePolicy, Dimensions, Lifecycle, MAX_AUTHORITY_TEXT_BYTES, MAX_COMMAND_BYTES,
    MAX_WRITE_BYTES, ReturnCondition, ReturnOutcome, SessionFacts, ShellSpec, StructuredErrorCode,
    TerminalFailure,
};
use crate::terminal_store::{
    DurableTerminal, NewTerminal, StoreError, TerminalRecord, TerminalStore, now_ms,
};

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
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub session: SessionFacts,
    pub outcome: ReturnOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    pub session: SessionFacts,
    pub output: Vec<u8>,
    pub total_bytes: u64,
    pub contains_escape: bool,
    pub output_incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenResult {
    pub session: SessionFacts,
    pub text: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteResult {
    pub session: SessionFacts,
    pub accepted_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectResult {
    pub session: SessionFacts,
    pub shell: String,
    pub cwd: String,
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseResult {
    pub session: SessionFacts,
    pub policy: ClosePolicy,
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
        if !valid_start(request) {
            return Err(refuse(StructuredErrorCode::InvalidRequest));
        }
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
                    timeout_ms: request.timeout_ms,
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
        let grid = Grid::new(DEFAULT_DIMENSIONS.columns, DEFAULT_DIMENSIONS.rows).ok();
        let session = Arc::new(Session::new(durable, grid));
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

    pub fn read(&self, owner: &str, session_id: &str) -> Result<ReadResult, TerminalFailure> {
        if let Some(session) = self.live(Action::Read, owner, session_id)? {
            let drained = session.drain();
            return Ok(ReadResult {
                session: session.facts(),
                output: drained.head,
                total_bytes: drained.total_bytes,
                contains_escape: drained.contains_escape,
                output_incomplete: drained.incomplete,
            });
        }
        let durable = self.saved(Action::Read, owner, session_id)?;
        Ok(ReadResult {
            session: saved_facts(durable.record()),
            output: Vec::new(),
            total_bytes: 0,
            contains_escape: false,
            output_incomplete: true,
        })
    }

    pub fn screen(&self, owner: &str, session_id: &str) -> Result<ScreenResult, TerminalFailure> {
        let unavailable = || {
            TerminalFailure::new(
                Action::Screen,
                StructuredErrorCode::ScreenUnavailable,
                Some(session_id),
            )
        };
        let Some(session) = self.live(Action::Screen, owner, session_id)? else {
            self.saved(Action::Screen, owner, session_id)?;
            return Err(unavailable());
        };
        Ok(ScreenResult {
            text: session.screen_text().ok_or_else(unavailable)?,
            session: session.facts(),
        })
    }

    pub fn write(
        &self,
        owner: &str,
        session_id: &str,
        text: &str,
    ) -> Result<WriteResult, TerminalFailure> {
        let failure = |code| TerminalFailure::new(Action::Write, code, Some(session_id));
        if text.is_empty() || text.len() > MAX_WRITE_BYTES {
            return Err(failure(StructuredErrorCode::InvalidRequest));
        }
        let session = self
            .live(Action::Write, owner, session_id)?
            .ok_or_else(|| failure(StructuredErrorCode::SessionNotFound))?;
        Ok(WriteResult {
            accepted_bytes: session.write(text.as_bytes()).map_err(failure)?,
            session: session.facts(),
        })
    }

    pub async fn wait(
        &self,
        owner: &str,
        session_id: &str,
        condition: ReturnCondition,
        ceiling: Duration,
        cancel: &CancellationToken,
    ) -> Result<Outcome, TerminalFailure> {
        let failure = |code| TerminalFailure::new(Action::Wait, code, Some(session_id));
        if ceiling.is_zero() {
            return Err(failure(StructuredErrorCode::InvalidRequest));
        }
        let Some(session) = self.live(Action::Wait, owner, session_id)? else {
            if condition != ReturnCondition::Exit {
                return Err(failure(StructuredErrorCode::SessionNotFound));
            }
            let durable = self.saved(Action::Wait, owner, session_id)?;
            let termination = durable
                .termination()
                .ok_or_else(|| failure(StructuredErrorCode::SessionNotFound))?;
            return Ok(Outcome {
                session: saved_facts(durable.record()),
                outcome: termination.into(),
            });
        };
        match session.wait_for(condition, ceiling, cancel).await {
            Err(SessionLost) => {
                session.finalize_backend().await;
                Err(failure(StructuredErrorCode::SessionLost))
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
        if let Some(session) = self.live(Action::Inspect, owner, session_id)? {
            return Ok(InspectResult {
                session: session.facts(),
                shell: session.shell.clone(),
                cwd: session.cwd.clone(),
                command: Some(session.command.clone()),
            });
        }
        let durable = self.saved(Action::Inspect, owner, session_id)?;
        let record = durable.record();
        Ok(InspectResult {
            session: saved_facts(record),
            shell: record.shell.clone(),
            cwd: record.cwd.clone(),
            command: record.command.clone(),
        })
    }

    pub fn list(&self, owner: &str) -> Result<Vec<SessionFacts>, TerminalFailure> {
        let records = self
            .registry
            .store
            .reconcile_owner(owner, now_ms())
            .map_err(|error| TerminalFailure::new(Action::List, error.code(), Some(owner)))?;
        Ok(records
            .iter()
            .map(|record| {
                self.registry
                    .find(&record.session_id)
                    .map_or_else(|| saved_facts(record), |session| session.facts())
            })
            .collect())
    }

    pub async fn close(
        &self,
        owner: &str,
        session_id: &str,
        policy: ClosePolicy,
    ) -> Result<CloseResult, TerminalFailure> {
        let failure = |code| TerminalFailure::new(Action::Close, code, Some(session_id));
        if let Some(session) = self.live(Action::Close, owner, session_id)? {
            let session = session.close(policy).await.map_err(failure)?;
            return Ok(CloseResult { session, policy });
        }
        let mut durable = self.saved(Action::Close, owner, session_id)?;
        durable
            .close(now_ms())
            .map_err(|error| failure(error.code()))?;
        Ok(CloseResult {
            session: saved_facts(durable.record()),
            policy,
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
    ) -> Result<Option<Arc<Session>>, TerminalFailure> {
        let failure = |code| TerminalFailure::new(action, code, Some(session_id));
        if !valid_session_id(session_id) {
            return Err(failure(StructuredErrorCode::InvalidRequest));
        }
        match self.registry.find(session_id) {
            Some(session) if session.owner != owner || session.is_closing() => {
                Err(failure(StructuredErrorCode::AuthorityDenied))
            }
            found => Ok(found),
        }
    }

    fn saved(
        &self,
        action: Action,
        owner: &str,
        session_id: &str,
    ) -> Result<DurableTerminal, TerminalFailure> {
        let failure = |code| TerminalFailure::new(action, code, Some(session_id));
        let durable = self
            .registry
            .store
            .open(owner, session_id)
            .map_err(|error| {
                failure(match error {
                    StoreError::OwnedByAnotherSession => StructuredErrorCode::AuthorityDenied,
                    _ => StructuredErrorCode::SessionNotFound,
                })
            })?;
        if durable.record().lifecycle == Lifecycle::Closed {
            return Err(failure(StructuredErrorCode::AuthorityDenied));
        }
        Ok(durable)
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

fn valid_start(request: &StartRequest<'_>) -> bool {
    let cwd = request.cwd.as_os_str().as_bytes();
    request.cwd.is_absolute()
        && valid_bounded_text(cwd, MAX_AUTHORITY_TEXT_BYTES)
        && valid_bounded_text(request.command.as_bytes(), MAX_COMMAND_BYTES)
        && !request.wait_ceiling.is_zero()
        && request
            .timeout_ms
            .is_none_or(|timeout| timeout != 0 && i64::try_from(timeout).is_ok())
}

fn valid_bounded_text(text: &[u8], maximum: usize) -> bool {
    !text.is_empty() && text.len() <= maximum && !text.contains(&0)
}

fn saved_facts(record: &TerminalRecord) -> SessionFacts {
    SessionFacts {
        session_id: record.session_id.clone(),
        lifecycle: record.lifecycle,
        timed_out: record.timed_out,
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
        Arc::new(Session::new(durable, None))
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
