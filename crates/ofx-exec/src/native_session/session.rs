use std::future;
use std::os::fd::OwnedFd;
use std::process::{Child, ChildStderr};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use super::control::{ControlKind, StartupFailure};
use super::launch::{ControlFrame, LaunchedTerminal, Liveness, read_control_frame};
use super::output::read_available;
use crate::terminal_contracts::{
    Lifecycle, ReturnCondition, ReturnOutcome, SessionFacts, StructuredErrorCode,
};
use crate::terminal_store::{DurableTerminal, Termination, now_ms};

const OUTPUT_POLL: Duration = Duration::from_millis(50);
const READ_BUFFER_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SessionLost;

pub(super) struct Session {
    pub(super) id: String,
    pub(super) owner: String,
    pub(super) shell: String,
    pub(super) cwd: String,
    pub(super) command: String,
    state: Mutex<State>,
    settled: Condvar,
    changed: watch::Sender<u64>,
}

struct State {
    lifecycle: Lifecycle,
    durable: DurableTerminal,
    term: Option<Termination>,
    start_failure: Option<StructuredErrorCode>,
    shell_group: Option<Pid>,
    shell_ready_seen: bool,
    ending_with_owner: bool,
    liveness: Option<Liveness>,
    backend: Backend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Reading,
    Draining,
    Finished,
}

impl State {
    fn persist_lost(&mut self) {
        let _ = self.durable.persist_lost(now_ms());
        self.lifecycle = Lifecycle::Lost;
        self.start_failure
            .get_or_insert(StructuredErrorCode::SessionLost);
    }
}

impl Session {
    pub(super) fn new(durable: DurableTerminal) -> Self {
        let record = durable.record();
        Self {
            id: record.session_id.clone(),
            owner: record.owner_session_id.clone(),
            shell: record.shell.clone(),
            cwd: record.cwd.clone(),
            command: record.command.clone().unwrap_or_default(),
            state: Mutex::new(State {
                lifecycle: record.lifecycle,
                durable,
                term: None,
                start_failure: None,
                shell_group: None,
                shell_ready_seen: false,
                ending_with_owner: false,
                liveness: None,
                backend: Backend::Finished,
            }),
            settled: Condvar::new(),
            changed: watch::Sender::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
        self.settled.notify_all();
    }

    pub(super) fn attach(
        self: &Arc<Self>,
        terminal: LaunchedTerminal,
    ) -> Result<(), StructuredErrorCode> {
        let LaunchedTerminal {
            master,
            control,
            liveness,
            launcher,
        } = terminal;
        let launcher = Launcher(launcher);
        let master = Arc::new(master);
        {
            let mut state = self.lock();
            state.liveness = Some(liveness);
            state.backend = Backend::Reading;
        }
        let output = {
            let session = Arc::clone(self);
            let master = Arc::clone(&master);
            thread::Builder::new().spawn(move || session.drain_output(&master))
        };
        let Ok(output) = output else {
            drop(launcher);
            self.finish_output();
            self.finish_backend();
            return Err(StructuredErrorCode::StartupFailed);
        };
        let control = {
            let session = Arc::clone(self);
            thread::Builder::new()
                .spawn(move || session.run_control(control, launcher, master, output))
        };
        if control.is_err() {
            self.wait_output_done();
            self.finish_backend();
            return Err(StructuredErrorCode::StartupFailed);
        }
        let mut state = self.lock();
        if !state.lifecycle.is_live() {
            return Err(state
                .start_failure
                .unwrap_or(StructuredErrorCode::SessionLost));
        }
        state
            .liveness
            .as_mut()
            .and_then(|liveness| liveness.release_launcher().ok())
            .ok_or(StructuredErrorCode::StartupFailed)
    }

    pub(super) fn abandon_unreleased(&self) {
        let mut state = self.lock();
        state.liveness = None;
        if state.durable.rollback_unreleased_start().is_ok() {
            state.lifecycle = Lifecycle::Lost;
            state.start_failure = Some(StructuredErrorCode::StartupFailed);
        } else if state.lifecycle.is_live() {
            state.persist_lost();
        }
        drop(state);
        self.notify();
    }

    pub(super) fn facts(&self) -> SessionFacts {
        let state = self.lock();
        let record = state.durable.record();
        SessionFacts {
            session_id: self.id.clone(),
            lifecycle: record.lifecycle,
            timed_out: record.timed_out,
        }
    }

    pub(super) fn is_recyclable(&self) -> bool {
        let state = self.lock();
        !state.lifecycle.is_live() && state.backend == Backend::Finished
    }

    pub(super) fn start_failure(&self) -> StructuredErrorCode {
        self.lock()
            .start_failure
            .unwrap_or(StructuredErrorCode::SessionLost)
    }

    pub(super) async fn wait_for(
        &self,
        condition: ReturnCondition,
        ceiling: Duration,
        cancel: &CancellationToken,
    ) -> Result<ReturnOutcome, SessionLost> {
        let deadline = Instant::now().checked_add(ceiling);
        let mut changes = self.changed.subscribe();
        loop {
            if cancel.is_cancelled() {
                return Ok(ReturnOutcome::Cancelled);
            }
            {
                let state = self.lock();
                if let Some(term) = state.term {
                    return Ok(term.into());
                }
                if condition == ReturnCondition::Started && state.lifecycle == Lifecycle::Running {
                    return Ok(ReturnOutcome::Started);
                }
                if state.lifecycle == Lifecycle::Lost {
                    return Err(SessionLost);
                }
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(ReturnOutcome::SafetyCeiling);
            }
            tokio::select! {
                () = cancel.cancelled() => {}
                _ = changes.changed() => {}
                () = sleep_until_or_never(deadline) => {}
            }
        }
    }

    pub(super) async fn finalize_backend(&self) {
        self.wait_until(None, |state| state.backend == Backend::Finished)
            .await;
    }

    pub(super) async fn finalize_backend_before(&self, deadline: Instant) -> bool {
        self.wait_until(Some(deadline), |state| state.backend == Backend::Finished)
            .await
    }

    pub(super) async fn settle_before(&self, deadline: Instant) -> bool {
        self.wait_until(Some(deadline), |state| !state.lifecycle.is_live())
            .await
    }

    async fn wait_until(&self, deadline: Option<Instant>, done: impl Fn(&State) -> bool) -> bool {
        let mut changes = self.changed.subscribe();
        loop {
            if done(&self.lock()) {
                return true;
            }
            match deadline {
                Some(deadline) => tokio::select! {
                    _ = changes.changed() => {}
                    () = sleep_until(deadline) => return done(&self.lock()),
                },
                None => {
                    let _ = changes.changed().await;
                }
            }
        }
    }

    pub(super) fn hang_up_for_exit(&self) -> bool {
        {
            let mut state = self.lock();
            if !state.lifecycle.is_live() {
                return false;
            }
            state.ending_with_owner = true;
        }
        self.signal_group(Signal::HUP);
        true
    }

    pub(super) fn shutdown(&self) {
        self.signal_group(Signal::KILL);
        self.lock().liveness = None;
    }

    pub(super) fn kill_for_exit(&self) {
        if self.lock().lifecycle.is_live() {
            self.signal_group(Signal::KILL);
            self.mark_lost();
        }
    }

    fn signal_group(&self, signal: Signal) {
        let group = {
            let state = self.lock();
            if !state.lifecycle.is_live() {
                return;
            }
            state.shell_group
        };
        if let Some(group) = group {
            let _ = kill_process_group(group, signal);
        }
    }

    fn drain_output(&self, master: &OwnedFd) {
        let mut buffer = vec![0; READ_BUFFER_BYTES];
        while read_available(master, &mut buffer, OUTPUT_POLL).is_ok() {}
        self.finish_output();
    }

    fn finish_output(&self) {
        let mut state = self.lock();
        if state.backend == Backend::Reading {
            state.backend = Backend::Draining;
        }
        drop(state);
        self.notify();
    }

    fn wait_output_done(&self) {
        let mut state = self.lock();
        while state.backend == Backend::Reading {
            state = self
                .settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn run_control(
        &self,
        mut control: ChildStderr,
        mut launcher: Launcher,
        master: Arc<OwnedFd>,
        output: thread::JoinHandle<()>,
    ) {
        loop {
            let Ok(frame) = read_control_frame(&mut control) else {
                if !self.reported() {
                    self.mark_lost();
                }
                break;
            };
            if matches!(
                frame.kind,
                ControlKind::CommandExited
                    | ControlKind::CommandSignal
                    | ControlKind::StartupFailed
                    | ControlKind::InvalidTerm
            ) {
                self.wait_output_done();
            }
            self.handle_control(frame);
        }
        let reported = {
            let state = self.lock();
            state.term.is_some() || state.lifecycle == Lifecycle::Lost
        };
        match launcher.0.wait() {
            Err(_) => self.mark_lost(),
            Ok(status) if !reported && !status.success() => self.mark_lost(),
            Ok(_) => {}
        }
        drop(master);
        self.wait_output_done();
        let _ = output.join();
        self.finish_backend();
    }

    fn finish_backend(&self) {
        let mut state = self.lock();
        state.liveness = None;
        state.backend = Backend::Finished;
        drop(state);
        self.notify();
    }

    fn reported(&self) -> bool {
        let state = self.lock();
        state.term.is_some() || state.start_failure.is_some()
    }

    fn handle_control(&self, frame: ControlFrame) {
        match frame.kind {
            ControlKind::Prepared => {}
            ControlKind::ShellReady => {
                let mut state = self.lock();
                if state.lifecycle != Lifecycle::Starting || state.shell_ready_seen {
                    state.lifecycle = Lifecycle::Lost;
                    state.start_failure = Some(StructuredErrorCode::SessionLost);
                } else {
                    state.shell_ready_seen = true;
                }
                drop(state);
                self.notify();
            }
            ControlKind::CommandStarted => self.command_started(frame.value),
            ControlKind::CommandExited => match i32::try_from(frame.value) {
                Ok(code) => self.set_term(Termination::Exited(code)),
                Err(_) => self.mark_lost(),
            },
            ControlKind::CommandSignal => {
                if (1..=255).contains(&frame.value) {
                    self.set_term(Termination::Signal(frame.value));
                } else {
                    self.mark_lost();
                }
            }
            ControlKind::StartupFailed => match frame.value {
                value if value == StartupFailure::ShellUnavailable as u32 => {
                    self.fail_closed(StructuredErrorCode::ShellUnavailable);
                }
                value
                    if value == StartupFailure::ProfileFailed as u32
                        || value == StartupFailure::ControlFailed as u32 =>
                {
                    self.fail_closed(StructuredErrorCode::StartupFailed);
                }
                _ => self.mark_lost(),
            },
            ControlKind::InvalidTerm => self.mark_lost(),
        }
    }

    fn command_started(&self, raw_pid: u32) {
        let valid = {
            let state = self.lock();
            state.lifecycle == Lifecycle::Starting
                && state.shell_ready_seen
                && state.backend == Backend::Reading
        };
        let shell = i32::try_from(raw_pid).ok().and_then(Pid::from_raw);
        let Some(shell) = shell.filter(|_| valid) else {
            return self.fail_closed(StructuredErrorCode::SessionLost);
        };
        {
            let mut state = self.lock();
            if state.lifecycle != Lifecycle::Starting || state.shell_group.is_some() {
                state.persist_lost();
            } else if state.durable.mark_started(raw_pid, now_ms()).is_ok() {
                state.shell_group = Some(shell);
                state.lifecycle = Lifecycle::Running;
            } else {
                state.persist_lost();
            }
            if state.lifecycle == Lifecycle::Lost {
                state.liveness = None;
            }
        }
        self.notify();
        if !self.release_command() {
            self.fail_closed(StructuredErrorCode::SessionLost);
        }
    }

    fn release_command(&self) -> bool {
        let mut state = self.lock();
        if state.lifecycle != Lifecycle::Running {
            return false;
        }
        state
            .liveness
            .as_mut()
            .is_some_and(|liveness| liveness.release_command().is_ok())
    }

    fn set_term(&self, term: Termination) {
        let mut state = self.lock();
        if state.ending_with_owner && state.lifecycle == Lifecycle::Running {
            state.term = Some(term);
            state.persist_lost();
            state.liveness = None;
        } else if state.lifecycle != Lifecycle::Running {
            if state.lifecycle == Lifecycle::Starting {
                state.persist_lost();
                state.start_failure = Some(StructuredErrorCode::StartupFailed);
            }
            state.liveness = None;
        } else if state.durable.persist_termination(term, now_ms()).is_err() {
            state.persist_lost();
            state.liveness = None;
        } else {
            state.term = Some(term);
            state.lifecycle = Lifecycle::Exited;
        }
        drop(state);
        self.notify();
    }

    fn fail_closed(&self, code: StructuredErrorCode) {
        let mut state = self.lock();
        if state.lifecycle.is_live() {
            state.persist_lost();
            state.start_failure = Some(code);
        }
        state.liveness = None;
        drop(state);
        self.notify();
    }

    fn mark_lost(&self) {
        let mut state = self.lock();
        if state.lifecycle.is_live() {
            state.persist_lost();
        }
        state.liveness = None;
        drop(state);
        self.notify();
    }
}

async fn sleep_until_or_never(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => future::pending().await,
    }
}

struct Launcher(Child);

impl Drop for Launcher {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
