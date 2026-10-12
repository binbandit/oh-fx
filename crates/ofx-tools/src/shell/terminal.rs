use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ofx_contract::{PathAccess, ToolOutput};
use ofx_exec::{
    Backend, ClosePolicy, Lifecycle, ManagedExecutions, Observation, Profile, ReturnCondition,
    ShellSpec, Snapshot, SnapshotState, StartRequest, SyncOutcome, TerminalFailure,
    TerminalObserver, TerminalSignal, Terminals, TtyCommand, TtyUpdate, check_terminal_shell,
    configured_login_shell, profile_shell, snapshot_state,
};
use ofx_workspace::path_inside;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::request::ShellRequest;
use super::snapshot_format::{
    command_result, format_snapshot_accepting, process_presentation, runtime_failure,
};
use super::{
    MAX_COMMAND_OUTPUT_BYTES, MAX_TOOL_RESULT_BYTES, PATH_OUTSIDE_WORKSPACE, ShellContext,
    UNAVAILABLE, WorkingDirectory, finish_command, finish_stop,
};

const READY_CEILING: Duration = Duration::from_secs(20);
const STOP_CEILING: Duration = Duration::from_secs(2);
const ECHO_SETTLE: Duration = Duration::from_millis(100);
const WAIT_PREEMPTED: &str = "WaitPreempted";
const EXECUTION_TERMINAL: &str = "ExecutionTerminal";
const AUTHORITY_UNAVAILABLE: &str = "TerminalAuthorityUnavailable";
const TERMINAL_NOT_READY: &str = "TerminalNotReady";
const TERMINAL_CLOSE_FAILED: &str = "TerminalCloseFailed";
const TERMINAL_ENDED: &str = "TerminalEnded";
const TERMINAL_ENDED_MESSAGE: &str = "This terminal ended when the oh-fx process that started it exited. It cannot be resumed. Start a new terminal with shell action run and tty true.";

#[derive(Clone, Default)]
pub struct TerminalOwner(Arc<Mutex<Option<String>>>);

impl TerminalOwner {
    pub fn bind(&self, owner_session_id: Option<String>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = owner_session_id;
    }

    fn current(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn is_bound(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

pub struct ShellTerminals {
    pub terminals: Terminals,
    pub owner: TerminalOwner,
    pub always_offered: bool,
}

impl ShellTerminals {
    pub(super) fn offered(&self) -> bool {
        self.always_offered || self.supported()
    }

    pub(super) fn supported(&self) -> bool {
        self.owner.is_bound()
    }
}

struct Bound<'a> {
    terminals: &'a Terminals,
    executions: &'a ManagedExecutions,
    owner: String,
    cancel: &'a CancellationToken,
}

impl Bound<'_> {
    fn observer(&self) -> TerminalObserver<'_> {
        TerminalObserver {
            terminals: self.terminals,
            executions: self.executions,
            owner: &self.owner,
            max_output_bytes: MAX_COMMAND_OUTPUT_BYTES,
            cancel: self.cancel,
        }
    }

    async fn finalize_completed(
        &self,
        session_id: &str,
        state: SnapshotState,
    ) -> Result<(), String> {
        if !matches!(state, SnapshotState::Completed(_)) {
            return Ok(());
        }
        self.terminals
            .close(&self.owner, session_id, ClosePolicy::Graceful)
            .await
            .map(drop)
            .map_err(|_| runtime_failure(TERMINAL_CLOSE_FAILED))
    }

    async fn close_best_effort(&self, session_id: &str) {
        let _ = self
            .terminals
            .close(&self.owner, session_id, ClosePolicy::Force)
            .await;
    }

    async fn write(&self, session_id: &str, chars: &str) -> Result<(u32, SnapshotState), String> {
        let terminals = self.terminals.clone();
        let owner = self.owner.clone();
        let session_id = session_id.to_owned();
        let chars = chars.to_owned();
        let written =
            tokio::task::spawn_blocking(move || terminals.write(&owner, &session_id, &chars))
                .await
                .map_err(|_| runtime_failure(UNAVAILABLE))?
                .map_err(|failure| terminal_failure(&failure))?;
        Ok((
            written.accepted_bytes,
            snapshot_state(&written.session, None),
        ))
    }
}

impl ShellContext {
    fn bound<'a>(&'a self, cancel: &'a CancellationToken) -> Option<Bound<'a>> {
        let terminals = self.terminals.as_ref()?;
        Some(Bound {
            terminals: &terminals.terminals,
            executions: &self.executions,
            owner: terminals.owner.current()?,
            cancel,
        })
    }

    pub(super) async fn run_terminal(
        &self,
        request: ShellRequest,
        cwd: WorkingDirectory,
        shell: Result<ShellSpec, String>,
        path_access: PathAccess,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        if let Some(root) = path_access.confining_root(&self.workspace_root)
            && !self
                .resolve_cwd(request.cwd.as_deref())
                .is_ok_and(|current| path_inside(root, &current))
        {
            return ToolOutput::failure(runtime_failure(PATH_OUTSIDE_WORKSPACE));
        }
        let (Some(bound), Some(command)) = (self.bound(cancel), request.command) else {
            return ToolOutput::failure(runtime_failure(UNAVAILABLE));
        };
        let shell = match shell.and_then(|shell| {
            check_terminal_shell(configured_login_shell().as_deref(), &shell)
                .map(|()| shell)
                .map_err(|error| error.to_string())
        }) {
            Ok(shell) => shell,
            Err(code) => return ToolOutput::failure(runtime_failure(&code)),
        };
        let (cwd, held) = match cwd.still_named() {
            Ok(held) => held,
            Err(code) => return ToolOutput::failure(runtime_failure(&code)),
        };
        let capacity = match self.executions.reserve_tty_capacity() {
            Ok(capacity) => capacity,
            Err(error) => return ToolOutput::failure(runtime_failure(&error.to_string())),
        };
        let yield_time = u64::from(request.yield_time_ms);
        let started = bound
            .terminals
            .start(
                &StartRequest {
                    owner_session_id: &bound.owner,
                    cwd: &cwd,
                    command: &command,
                    shell: &shell,
                    return_when: if yield_time == 0 {
                        ReturnCondition::Started
                    } else {
                        ReturnCondition::Exit
                    },
                    wait_ceiling: Duration::from_millis(yield_time.max(1)),
                    timeout_ms: request.timeout_ms,
                },
                cancel,
            )
            .await;
        drop(held);
        let started = match started {
            Ok(started) => started,
            Err(failure) => return ToolOutput::failure(terminal_failure(&failure)),
        };
        let session_id = started.session.session_id.clone();
        let registered = async {
            let observation = bound
                .observer()
                .observe(
                    &session_id,
                    snapshot_state(&started.session, Some(started.outcome)),
                )
                .await
                .map_err(|error| runtime_failure(&error.to_string()))?;
            bound
                .finalize_completed(&session_id, observation.state)
                .await?;
            self.executions
                .register_tty(
                    Some(capacity),
                    &TtyCommand {
                        command: &command,
                        cwd: &cwd,
                        max_output_bytes: MAX_COMMAND_OUTPUT_BYTES,
                    },
                    &observation.update(&session_id, observation.state == SnapshotState::Running),
                )
                .map_err(|error| runtime_failure(&error.to_string()))
        }
        .await;
        match registered {
            Ok(snapshot) => finish_command(&snapshot),
            Err(failure) => {
                bound.close_best_effort(&session_id).await;
                ToolOutput::failure(failure)
            }
        }
    }

    pub(super) async fn index_terminal(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), ToolOutput> {
        if self.executions.state_for(session_id).is_some() {
            return Ok(());
        }
        let Some(bound) = self.bound(cancel) else {
            return Ok(());
        };
        match bound.observer().sync_owned(session_id).await {
            Ok(SyncOutcome::Indexed) => Ok(()),
            Ok(SyncOutcome::EndedWithOwner) => Err(ToolOutput::failure(terminal_ended(session_id))),
            Err(error) => Err(ToolOutput::failure(runtime_failure(&error.to_string()))),
        }
    }

    pub(super) fn is_terminal(&self, session_id: &str) -> bool {
        self.executions.backend_for(session_id) == Some(Backend::Tty)
    }

    pub(super) async fn interact_terminal(
        &self,
        session_id: &str,
        chars: &str,
        yield_time_ms: u32,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        if self.executions.is_tombstone(session_id) {
            return ToolOutput::failure(runtime_failure(EXECUTION_TERMINAL));
        }
        let Some(bound) = self.bound(cancel) else {
            return ToolOutput::failure(runtime_failure(AUTHORITY_UNAVAILABLE));
        };
        let mut state = SnapshotState::Running;
        let mut accepted_bytes = None;
        if !chars.is_empty() {
            match bound
                .terminals
                .wait(
                    &bound.owner,
                    session_id,
                    ReturnCondition::Started,
                    READY_CEILING,
                    cancel,
                )
                .await
            {
                Err(failure) => return ToolOutput::failure(terminal_failure(&failure)),
                Ok(ready) if ready.session.lifecycle != Lifecycle::Running => {
                    return ToolOutput::failure(runtime_failure(TERMINAL_NOT_READY));
                }
                Ok(_) => {}
            }
            match bound.write(session_id, chars).await {
                Ok((accepted, written)) => {
                    accepted_bytes = Some(accepted);
                    state = written;
                }
                Err(failure) => return ToolOutput::failure(failure),
            }
        }
        let reservation = match self.executions.reserve_external_wait(session_id) {
            Ok(waiter) => ExternalWait {
                executions: &self.executions,
                session_id,
                waiter,
            },
            Err(error) => return ToolOutput::failure(runtime_failure(&error.to_string())),
        };
        if state == SnapshotState::Running && yield_time_ms != 0 {
            match bound
                .terminals
                .wait(
                    &bound.owner,
                    session_id,
                    ReturnCondition::Exit,
                    Duration::from_millis(yield_time_ms.into()),
                    cancel,
                )
                .await
            {
                Ok(waited) => state = snapshot_state(&waited.session, Some(waited.outcome)),
                Err(failure) => return ToolOutput::failure(terminal_failure(&failure)),
            }
        }
        if accepted_bytes.is_some() && yield_time_ms == 0 {
            tokio::time::sleep(ECHO_SETTLE).await;
        }
        if reservation.preempted() {
            return ToolOutput::failure(runtime_failure(WAIT_PREEMPTED));
        }
        let observer = bound.observer();
        let observation = match observer.observe(session_id, state).await {
            Ok(observation) => observation,
            Err(error) => return ToolOutput::failure(runtime_failure(&error.to_string())),
        };
        let kept = |failure: String| {
            observer.keep(session_id, &observation);
            ToolOutput::failure(failure)
        };
        if reservation.preempted() {
            return kept(runtime_failure(WAIT_PREEMPTED));
        }
        if let Err(failure) = bound
            .finalize_completed(session_id, observation.state)
            .await
        {
            return kept(failure);
        }
        let snapshot = match self
            .executions
            .update_tty(&observation.update(session_id, true))
        {
            Ok(snapshot) => snapshot,
            Err(error) => return kept(runtime_failure(&error.to_string())),
        };
        drop(reservation);
        match accepted_bytes {
            Some(accepted) => finish_accepted(&snapshot, accepted),
            None => finish_command(&snapshot),
        }
    }

    pub(super) async fn stop_terminal(
        &self,
        session_id: &str,
        force: bool,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let bound = self.bound(cancel);
        if let Some(state) = self.settled_state(session_id) {
            return self
                .finish_terminal_stop(bound.as_ref(), session_id, state)
                .await;
        }
        let Some(bound) = bound else {
            return ToolOutput::failure(runtime_failure(AUTHORITY_UNAVAILABLE));
        };
        if let Err(error) = bound.observer().refresh(session_id).await {
            return ToolOutput::failure(runtime_failure(&error.to_string()));
        }
        if let Some(state) = self.settled_state(session_id) {
            return self
                .finish_terminal_stop(Some(&bound), session_id, state)
                .await;
        }
        self.executions.preempt_wait(session_id);
        let signal = if force {
            TerminalSignal::Kill
        } else {
            TerminalSignal::Terminate
        };
        if let Err(failure) = bound.terminals.signal(&bound.owner, session_id, signal) {
            return ToolOutput::failure(terminal_failure(&failure));
        }
        let waited = match bound
            .terminals
            .wait(
                &bound.owner,
                session_id,
                ReturnCondition::Exit,
                STOP_CEILING,
                cancel,
            )
            .await
        {
            Ok(waited) => waited,
            Err(failure) => return ToolOutput::failure(terminal_failure(&failure)),
        };
        let observation = match bound
            .observer()
            .observe(
                session_id,
                snapshot_state(&waited.session, Some(waited.outcome)),
            )
            .await
        {
            Ok(observation) => observation,
            Err(error) => return ToolOutput::failure(runtime_failure(&error.to_string())),
        };
        let policy = if force {
            ClosePolicy::Force
        } else {
            ClosePolicy::Graceful
        };
        if let Err(failure) = bound
            .terminals
            .close(&bound.owner, session_id, policy)
            .await
        {
            bound.observer().keep(session_id, &observation);
            return ToolOutput::failure(terminal_failure(&failure));
        }
        let stopped = Observation {
            state: SnapshotState::Stopped(waited.outcome.status()),
            ..observation
        };
        self.deliver_stop(&stopped.update(session_id, true))
    }

    fn settled_state(&self, session_id: &str) -> Option<SnapshotState> {
        self.executions
            .state_for(session_id)
            .filter(|state| *state != SnapshotState::Running)
    }

    async fn finish_terminal_stop(
        &self,
        bound: Option<&Bound<'_>>,
        session_id: &str,
        state: SnapshotState,
    ) -> ToolOutput {
        if matches!(state, SnapshotState::Completed(_)) {
            let Some(bound) = bound else {
                return ToolOutput::failure(runtime_failure(AUTHORITY_UNAVAILABLE));
            };
            if let Err(failure) = bound.finalize_completed(session_id, state).await {
                return ToolOutput::failure(failure);
            }
        }
        self.deliver_stop(&TtyUpdate {
            execution_id: session_id,
            state,
            output: b"",
            raw_bytes: 0,
            output_incomplete: false,
            error_name: None,
            published_running: true,
        })
    }

    fn deliver_stop(&self, update: &TtyUpdate<'_>) -> ToolOutput {
        match self.executions.update_tty(update) {
            Ok(snapshot) => finish_stop(&snapshot),
            Err(error) => ToolOutput::failure(runtime_failure(&error.to_string())),
        }
    }
}

struct ExternalWait<'a> {
    executions: &'a ManagedExecutions,
    session_id: &'a str,
    waiter: u64,
}

impl ExternalWait<'_> {
    fn preempted(&self) -> bool {
        self.executions
            .external_wait_preempted(self.session_id, self.waiter)
    }
}

impl Drop for ExternalWait<'_> {
    fn drop(&mut self) {
        self.executions
            .release_external_wait(self.session_id, self.waiter);
    }
}

fn finish_accepted(snapshot: &Snapshot, accepted_bytes: u32) -> ToolOutput {
    ToolOutput::success(format_snapshot_accepting(
        snapshot,
        Some(accepted_bytes),
        MAX_TOOL_RESULT_BYTES,
    ))
    .with_command_result(command_result(snapshot))
    .with_process(process_presentation(snapshot))
}

pub(super) fn terminal_failure(failure: &TerminalFailure) -> String {
    json!({"failure": {
        "action": failure.action.name(),
        "code": failure.code.name(),
        "session_id": failure.session_id,
        "retryable": false,
    }})
    .to_string()
}

pub(super) fn terminal_ended(session_id: &str) -> String {
    json!({"error": {
        "tool": "shell",
        "code": TERMINAL_ENDED,
        "session_id": session_id,
        "message": TERMINAL_ENDED_MESSAGE,
        "retryable": false,
    }})
    .to_string()
}

pub(super) fn terminal_shell(
    request: &ShellRequest,
    configured_login_shell: Option<&Path>,
) -> Result<ShellSpec, String> {
    if let Some(shell) = &request.shell {
        return Ok(ShellSpec::Executable {
            path: shell.path.clone().into(),
            clean_start: shell.clean_start,
        });
    }
    profile_shell(
        configured_login_shell,
        request.profile.unwrap_or(Profile::User),
    )
    .map_err(|error| error.to_string())
}
