use std::future;
use std::os::fd::OwnedFd;
use std::process::{Child, ChildStderr};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use ofx_vt::Grid;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use super::control::{ControlKind, StartupFailure};
use super::launch::{ControlFrame, LaunchedTerminal, Liveness, read_control_frame};
use super::output::{Drained, Unread, read_available};
use crate::terminal_contracts::{
    ClosePolicy, Lifecycle, ReturnCondition, ReturnOutcome, SessionFacts, StructuredErrorCode,
};
use crate::terminal_store::{DurableTerminal, StoreError, Termination, now_ms};

const OUTPUT_POLL: Duration = Duration::from_millis(50);
const READ_BUFFER_BYTES: usize = 256 * 1024;
const GRACEFUL_CLOSE: Duration = Duration::from_millis(800);
const WRITE_POLL: Duration = Duration::from_millis(50);

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
    writes: Mutex<()>,
}

struct State {
    lifecycle: Lifecycle,
    durable: DurableTerminal,
    term: Option<Termination>,
    start_failure: Option<StructuredErrorCode>,
    shell_group: Option<Pid>,
    shell_ready_seen: bool,
    close_committed: bool,
    ending_with_owner: bool,
    unread: Unread,
    grid: Option<Grid>,
    master: Option<Arc<OwnedFd>>,
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
        if !self.close_committed {
            let _ = self.durable.persist_lost(now_ms());
        }
        self.lifecycle = Lifecycle::Lost;
        self.start_failure
            .get_or_insert(StructuredErrorCode::SessionLost);
    }
}

impl Session {
    pub(super) fn new(durable: DurableTerminal, grid: Option<Grid>) -> Self {
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
                close_committed: false,
                ending_with_owner: false,
                unread: Unread::default(),
                grid,
                master: None,
                liveness: None,
                backend: Backend::Finished,
            }),
            settled: Condvar::new(),
            changed: watch::Sender::new(0),
            writes: Mutex::new(()),
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
            state.master = Some(Arc::clone(&master));
            state.liveness = Some(liveness);
            state.backend = Backend::Reading;
        }
        let output = {
            let session = Arc::clone(self);
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
            thread::Builder::new().spawn(move || session.run_control(control, launcher, output))
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

    fn is_live(&self) -> bool {
        self.lock().lifecycle.is_live()
    }

    pub(super) fn drain(&self) -> Drained {
        self.lock().unread.drain()
    }

    pub(super) fn screen_text(&self) -> Option<Vec<u8>> {
        self.lock().grid.as_ref().map(Grid::screen_text)
    }

    pub(super) fn write(&self, bytes: &[u8]) -> Result<u32, StructuredErrorCode> {
        let _writes = self.writes.lock().unwrap_or_else(PoisonError::into_inner);
        let master = {
            let state = self.lock();
            input_open(&state)?;
            state
                .master
                .clone()
                .ok_or(StructuredErrorCode::InvalidLifecycle)?
        };
        self.write_all(&master, bytes)?;
        u32::try_from(bytes.len()).map_err(|_| StructuredErrorCode::InvalidRequest)
    }

    fn write_all(&self, master: &OwnedFd, mut bytes: &[u8]) -> Result<(), StructuredErrorCode> {
        let wait = Timespec::try_from(WRITE_POLL).unwrap_or_default();
        while !bytes.is_empty() {
            let written = {
                let state = self.lock();
                input_open(&state)?;
                rustix::io::write(master, bytes)
            };
            match written {
                Ok(0) => return Err(StructuredErrorCode::InvalidRequest),
                Ok(count) => bytes = &bytes[count..],
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => {
                    let mut fds = [PollFd::new(master, PollFlags::OUT)];
                    match poll(&mut fds, Some(&wait)) {
                        Ok(_) | Err(Errno::INTR) => {}
                        Err(_) => return Err(StructuredErrorCode::InvalidRequest),
                    }
                }
                Err(_) => {
                    input_open(&self.lock())?;
                    return Err(StructuredErrorCode::InvalidRequest);
                }
            }
        }
        Ok(())
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
                if !state.lifecycle.is_live() {
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

    pub(super) fn is_closing(&self) -> bool {
        self.lock().close_committed
    }

    pub(super) async fn close(
        &self,
        policy: ClosePolicy,
    ) -> Result<SessionFacts, StructuredErrorCode> {
        {
            let mut state = self.lock();
            if state.close_committed {
                return Err(StructuredErrorCode::AuthorityDenied);
            }
            state.close_committed = true;
        }
        let mut still_live = self.is_live();
        if still_live {
            let signal = match policy {
                ClosePolicy::Graceful => Signal::TERM,
                ClosePolicy::Force => Signal::KILL,
            };
            if !self.signal_group(signal) {
                self.mark_lost();
            }
        }
        if policy == ClosePolicy::Graceful && still_live {
            self.settle_before(Instant::now() + GRACEFUL_CLOSE).await;
            still_live = self.is_live();
            if still_live && !self.signal_group(Signal::KILL) {
                self.mark_lost();
            }
        }
        if still_live && !self.settle_before(Instant::now() + GRACEFUL_CLOSE).await {
            self.mark_lost();
        }
        self.finalize_backend().await;
        self.lock()
            .durable
            .close(now_ms())
            .map_err(StoreError::code)?;
        self.lock().lifecycle = Lifecycle::Closed;
        self.notify();
        Ok(self.facts())
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

    fn signal_group(&self, signal: Signal) -> bool {
        let group = {
            let state = self.lock();
            if !state.lifecycle.is_live() {
                return false;
            }
            state.shell_group
        };
        group.is_some_and(|group| kill_process_group(group, signal).is_ok())
    }

    fn drain_output(&self, master: &OwnedFd) {
        let mut buffer = vec![0; READ_BUFFER_BYTES];
        while let Ok(count) = read_available(master, &mut buffer, OUTPUT_POLL) {
            if count != 0 {
                self.append_output(&buffer[..count]);
            }
        }
        self.finish_output();
    }

    fn append_output(&self, bytes: &[u8]) {
        let mut state = self.lock();
        state.unread.append(bytes);
        if let Some(grid) = state.grid.as_mut()
            && grid.feed(bytes).is_err()
        {
            state.grid = None;
        }
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
        self: &Arc<Self>,
        mut control: ChildStderr,
        mut launcher: Launcher,
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
        self.wait_output_done();
        let _ = output.join();
        self.finish_backend();
    }

    fn finish_backend(&self) {
        let mut state = self.lock();
        state.master = None;
        state.liveness = None;
        state.backend = Backend::Finished;
        drop(state);
        self.notify();
    }

    fn reported(&self) -> bool {
        let state = self.lock();
        state.term.is_some() || state.start_failure.is_some()
    }

    fn handle_control(self: &Arc<Self>, frame: ControlFrame) {
        if self.lock().close_committed
            && !matches!(
                frame.kind,
                ControlKind::CommandExited | ControlKind::CommandSignal
            )
        {
            return;
        }
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

    fn command_started(self: &Arc<Self>, raw_pid: u32) {
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
        if let Some(deadline_ms) = self.timeout_deadline() {
            let session = Arc::clone(self);
            let watcher = thread::Builder::new().spawn(move || session.expire_at(deadline_ms));
            if watcher.is_err() {
                self.mark_lost();
            }
        }
        if !self.release_command() {
            self.fail_closed(StructuredErrorCode::SessionLost);
        }
    }

    fn timeout_deadline(&self) -> Option<i64> {
        let state = self.lock();
        if state.lifecycle == Lifecycle::Running {
            state.durable.record().timeout_at_ms
        } else {
            None
        }
    }

    fn expire_at(&self, deadline_ms: i64) {
        let mut state = self.lock();
        loop {
            if !state.lifecycle.is_live() || state.ending_with_owner {
                return;
            }
            let remaining = deadline_ms.saturating_sub(now_ms());
            let Some(remaining) = u64::try_from(remaining).ok().filter(|&ms| ms != 0) else {
                break;
            };
            state = self
                .settled
                .wait_timeout(state, Duration::from_millis(remaining))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let _ = state.durable.mark_timed_out(now_ms());
        drop(state);
        self.notify();
        if !self.signal_group(Signal::KILL) {
            self.mark_lost();
        }
    }

    fn release_command(&self) -> bool {
        let _writes = self.writes.lock().unwrap_or_else(PoisonError::into_inner);
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
        if state.close_committed {
            state.term = Some(term);
            state.lifecycle = Lifecycle::Closed;
            state.liveness = None;
        } else if state.ending_with_owner && state.lifecycle == Lifecycle::Running {
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

fn input_open(state: &State) -> Result<(), StructuredErrorCode> {
    if state.close_committed {
        Err(StructuredErrorCode::AuthorityDenied)
    } else if state.lifecycle != Lifecycle::Running {
        Err(StructuredErrorCode::InvalidLifecycle)
    } else {
        Ok(())
    }
}

struct Launcher(Child);

impl Drop for Launcher {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
