use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::command_contract::CommandStatus;
use crate::command_environment::Environment;
use crate::command_runner::{
    CapturedCommand, CapturedOutcome, OutputStream, RunError, SessionSupervisor, StopIntent,
    TERMINATION_SETTLE_TIMEOUT, run_captured,
};
use crate::directory_identity::DirectoryIdentity;
use crate::output_echo::{LineEcho, OutputEcho};
use crate::shell_resolver::captured_invocation;

const MAX_LIVE_ENTRIES: usize = 64;
const MAX_TOMBSTONES: usize = 32;
const STOP_SETTLE_TIMEOUT: Duration =
    TERMINATION_SETTLE_TIMEOUT.saturating_add(Duration::from_secs(1));
const STOP_SETTLEMENT_TIMED_OUT: &str = "StopSettlementTimedOut";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExecutionError {
    #[error("ExecutionNotFound")]
    ExecutionNotFound,
    #[error("ExecutionCapacityExceeded")]
    ExecutionCapacityExceeded,
    #[error("RuntimeStopping")]
    RuntimeStopping,
    #[error("Cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartCaptured {
    pub command: String,
    pub cwd: PathBuf,
    pub cwd_identity: DirectoryIdentity,
    pub environment: Environment,
    pub max_output_bytes: usize,
    pub timeout: Option<Duration>,
    pub yield_time: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotState {
    Running,
    Completed(CommandStatus),
    Stopped(Option<CommandStatus>),
    Lost,
}

impl SnapshotState {
    pub fn status(self) -> Option<CommandStatus> {
        match self {
            Self::Running => None,
            Self::Completed(status) => Some(status),
            Self::Stopped(status) => status,
            Self::Lost => Some(CommandStatus::Indeterminate),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub execution_id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub retained: bool,
    pub state: SnapshotState,
    pub output_delta: Vec<u8>,
    pub output_truncated: bool,
    pub output_incomplete: bool,
    pub duration_ms: Option<u64>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub error_name: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Stopping,
    Completed(CommandStatus),
    Stopped(Option<CommandStatus>),
    Lost,
}

impl Phase {
    fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::Stopping)
    }

    fn snapshot_state(self) -> SnapshotState {
        match self {
            Self::Running | Self::Stopping => SnapshotState::Running,
            Self::Completed(status) => SnapshotState::Completed(status),
            Self::Stopped(status) => SnapshotState::Stopped(status),
            Self::Lost => SnapshotState::Lost,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    Starting,
    Published,
    Abandoned,
}

struct EntryState {
    phase: Phase,
    output: Vec<u8>,
    max_output_bytes: usize,
    output_truncated: bool,
    output_incomplete: bool,
    duration_ms: Option<u64>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    error_name: Option<&'static str>,
    claim: Claim,
    tombstone: Option<u64>,
}

struct Entry {
    id: String,
    command: String,
    cwd: PathBuf,
    cwd_identity: DirectoryIdentity,
    state: Mutex<EntryState>,
    stop: watch::Sender<Option<StopIntent>>,
    finished: watch::Sender<bool>,
}

impl Entry {
    fn state(&self) -> MutexGuard<'_, EntryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn append(&self, stream: OutputStream, bytes: &[u8]) {
        let mut state = self.state();
        match stream {
            OutputStream::Stdout => {
                state.stdout_bytes = state.stdout_bytes.saturating_add(bytes.len());
            }
            OutputStream::Stderr => {
                state.stderr_bytes = state.stderr_bytes.saturating_add(bytes.len());
            }
        }
        let available = state.max_output_bytes.saturating_sub(state.output.len());
        let retained = available.min(bytes.len());
        state.output.extend_from_slice(&bytes[..retained]);
        if retained != bytes.len() {
            state.output_truncated = true;
        }
    }

    fn request_stop(&self, intent: StopIntent) {
        let mut state = self.state();
        if state.phase.is_terminal() {
            return;
        }
        state.phase = Phase::Stopping;
        drop(state);
        self.stop.send_if_modified(|requested| {
            if requested.is_some_and(|current| current >= intent) {
                return false;
            }
            *requested = Some(intent);
            true
        });
    }

    fn record(&self, result: Result<CapturedOutcome, RunError>) -> bool {
        let mut state = self.state();
        match result {
            Ok(outcome) => {
                state.phase = match state.phase {
                    Phase::Stopping => Phase::Stopped(Some(outcome.status)),
                    _ => Phase::Completed(outcome.status),
                };
                state.output_incomplete |= outcome.output_incomplete;
                state.duration_ms =
                    Some(u64::try_from(outcome.duration.as_millis()).unwrap_or(u64::MAX));
            }
            Err(RunError::TimeoutExpired) => {
                state.phase = Phase::Stopped(None);
                state.error_name = Some(RunError::TimeoutExpired.name());
            }
            Err(error) => {
                state.phase = Phase::Lost;
                state.error_name = Some(error.name());
            }
        }
        state.claim == Claim::Abandoned
    }

    fn is_terminal(&self) -> bool {
        self.state().phase.is_terminal()
    }

    async fn finished(&self) {
        let mut finished = self.finished.subscribe();
        let _ = finished.wait_for(|done| *done).await;
    }
}

#[derive(Default)]
struct Registry {
    entries: Vec<Arc<Entry>>,
    next_id: u64,
    next_tombstone: u64,
    stopping: bool,
}

struct Shared {
    supervisor: SessionSupervisor,
    registry: Mutex<Registry>,
}

impl Shared {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn settle(&self, entry: &Arc<Entry>, result: Result<CapturedOutcome, RunError>) {
        let mut registry = self.registry();
        if entry.record(result) {
            forget(&mut registry, entry);
        }
        drop(registry);
        entry.finished.send_replace(true);
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        for entry in &self.registry().entries {
            entry.request_stop(StopIntent::Force);
        }
    }
}

#[derive(Clone)]
pub struct ManagedExecutions {
    shared: Arc<Shared>,
    echo: Option<OutputEcho>,
}

impl ManagedExecutions {
    pub fn new(supervisor: SessionSupervisor) -> Self {
        Self {
            shared: Arc::new(Shared {
                supervisor,
                registry: Mutex::new(Registry {
                    next_id: 1,
                    ..Registry::default()
                }),
            }),
            echo: None,
        }
    }

    #[must_use]
    pub fn with_output_echo(mut self, echo: OutputEcho) -> Self {
        self.echo = Some(echo);
        self
    }

    pub async fn start_captured(
        &self,
        input: StartCaptured,
        cancel: &CancellationToken,
    ) -> Result<Snapshot, ExecutionError> {
        let entry = self.admit(&input)?;
        let invocation = captured_invocation(&input.environment, &input.command);
        if input.yield_time.is_zero() {
            let snapshot = self.deliver(&entry, true);
            self.spawn_driver(&entry, invocation, input.timeout);
            return Ok(snapshot);
        }
        self.spawn_driver(&entry, invocation, input.timeout);
        let unpublished = UnpublishedRun {
            shared: Arc::clone(&self.shared),
            entry: Arc::clone(&entry),
            armed: true,
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                entry.request_stop(StopIntent::Graceful);
                entry.finished().await;
            }
            _ = timeout(input.yield_time, entry.finished()) => {}
        }
        let snapshot = self.deliver(&entry, true);
        unpublished.disarm();
        Ok(snapshot)
    }

    pub async fn wait(
        &self,
        execution_id: &str,
        ceiling: Duration,
        cancel: &CancellationToken,
    ) -> Result<Snapshot, ExecutionError> {
        let entry = self.find(execution_id)?;
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ExecutionError::Cancelled),
            _ = timeout(ceiling, entry.finished()) => {}
        }
        Ok(self.deliver(&entry, false))
    }

    pub async fn stop(&self, execution_id: &str, force: bool) -> Result<Snapshot, ExecutionError> {
        let entry = self.find(execution_id)?;
        entry.request_stop(if force {
            StopIntent::Force
        } else {
            StopIntent::Graceful
        });
        if timeout(STOP_SETTLE_TIMEOUT, entry.finished()).await.is_ok() {
            return Ok(self.deliver(&entry, false));
        }
        let mut snapshot = self.deliver(&entry, false);
        if snapshot.state == SnapshotState::Running {
            snapshot.state = SnapshotState::Lost;
            snapshot.output_incomplete = true;
            snapshot.error_name = Some(STOP_SETTLEMENT_TIMED_OUT);
        }
        Ok(snapshot)
    }

    pub fn tombstone_snapshot(&self, execution_id: &str) -> Option<Snapshot> {
        let entry = self.find(execution_id).ok()?;
        entry.state().tombstone?;
        Some(self.deliver(&entry, false))
    }

    pub fn command(&self, execution_id: &str) -> Option<String> {
        self.find(execution_id)
            .ok()
            .map(|entry| entry.command.clone())
    }

    pub async fn shutdown(&self) {
        let live: Vec<Arc<Entry>> = {
            let mut registry = self.shared.registry();
            registry.stopping = true;
            registry
                .entries
                .iter()
                .filter(|entry| !entry.is_terminal())
                .cloned()
                .collect()
        };
        for entry in &live {
            entry.request_stop(StopIntent::Graceful);
        }
        let deadline = Instant::now() + STOP_SETTLE_TIMEOUT;
        for entry in live {
            let _ = timeout_at(deadline, entry.finished()).await;
        }
    }

    fn admit(&self, input: &StartCaptured) -> Result<Arc<Entry>, ExecutionError> {
        let mut registry = self.shared.registry();
        if registry.stopping {
            return Err(ExecutionError::RuntimeStopping);
        }
        let live = registry
            .entries
            .iter()
            .filter(|entry| entry.state().tombstone.is_none())
            .count();
        if live >= MAX_LIVE_ENTRIES {
            return Err(ExecutionError::ExecutionCapacityExceeded);
        }
        evict_oldest_tombstone(&mut registry);
        let id = format!("shell-{}", registry.next_id);
        registry.next_id = registry.next_id.checked_add(1).unwrap_or(1);
        let entry = Arc::new(Entry {
            id,
            command: input.command.clone(),
            cwd: input.cwd.clone(),
            cwd_identity: input.cwd_identity,
            state: Mutex::new(EntryState {
                phase: Phase::Running,
                output: Vec::new(),
                max_output_bytes: input.max_output_bytes,
                output_truncated: false,
                output_incomplete: false,
                duration_ms: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
                error_name: None,
                claim: Claim::Starting,
                tombstone: None,
            }),
            stop: watch::Sender::new(None),
            finished: watch::Sender::new(false),
        });
        registry.entries.push(Arc::clone(&entry));
        Ok(entry)
    }

    fn spawn_driver(&self, entry: &Arc<Entry>, invocation: Vec<OsString>, limit: Option<Duration>) {
        let entry = Arc::clone(entry);
        let shared = Arc::downgrade(&self.shared);
        let supervisor = self.shared.supervisor.clone();
        let mut echo = self.echo.clone().map(LineEcho::new);
        tokio::spawn(async move {
            let deadline = limit.and_then(|limit| Instant::now().checked_add(limit));
            let mut stop = entry.stop.subscribe();
            let mut sink = |stream, bytes: &[u8]| {
                entry.append(stream, bytes);
                if let Some(echo) = echo.as_mut() {
                    echo.push(stream, bytes);
                }
            };
            let command = CapturedCommand {
                argv: &invocation,
                cwd: &entry.cwd,
                cwd_identity: entry.cwd_identity,
                deadline,
                supervisor: &supervisor,
            };
            let result = run_captured(command, &mut stop, &mut sink).await;
            if let Some(echo) = echo.as_mut() {
                echo.flush();
            }
            settle(&shared, &entry, result);
        });
    }

    fn find(&self, execution_id: &str) -> Result<Arc<Entry>, ExecutionError> {
        self.shared
            .registry()
            .entries
            .iter()
            .find(|entry| entry.id == execution_id)
            .cloned()
            .ok_or(ExecutionError::ExecutionNotFound)
    }

    fn deliver(&self, entry: &Arc<Entry>, publish: bool) -> Snapshot {
        let mut registry = self.shared.registry();
        let mut state = entry.state();
        let terminal = state.phase.is_terminal();
        let snapshot = Snapshot {
            execution_id: entry.id.clone(),
            command: entry.command.clone(),
            cwd: entry.cwd.clone(),
            retained: state.claim == Claim::Published || !terminal,
            state: state.phase.snapshot_state(),
            output_delta: std::mem::take(&mut state.output),
            output_truncated: std::mem::replace(&mut state.output_truncated, false),
            output_incomplete: state.output_incomplete,
            duration_ms: state.duration_ms,
            stdout_bytes: state.stdout_bytes,
            stderr_bytes: state.stderr_bytes,
            error_name: state.error_name,
        };
        if !terminal {
            if publish {
                state.claim = Claim::Published;
            }
        } else if state.claim != Claim::Published {
            drop(state);
            forget(&mut registry, entry);
        } else if state.tombstone.is_none() {
            state.tombstone = Some(registry.next_tombstone);
            registry.next_tombstone += 1;
        }
        snapshot
    }
}

fn settle(shared: &Weak<Shared>, entry: &Arc<Entry>, result: Result<CapturedOutcome, RunError>) {
    if let Some(shared) = shared.upgrade() {
        shared.settle(entry, result);
    } else {
        entry.record(result);
        entry.finished.send_replace(true);
    }
}

fn forget(registry: &mut Registry, entry: &Arc<Entry>) {
    registry
        .entries
        .retain(|candidate| !Arc::ptr_eq(candidate, entry));
}

struct UnpublishedRun {
    shared: Arc<Shared>,
    entry: Arc<Entry>,
    armed: bool,
}

impl UnpublishedRun {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for UnpublishedRun {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut registry = self.shared.registry();
        let mut state = self.entry.state();
        if state.claim == Claim::Published {
            return;
        }
        state.claim = Claim::Abandoned;
        let settled = state.phase.is_terminal();
        drop(state);
        if settled {
            forget(&mut registry, &self.entry);
            return;
        }
        drop(registry);
        self.entry.request_stop(StopIntent::Graceful);
    }
}

fn evict_oldest_tombstone(registry: &mut Registry) {
    let tombstones: Vec<(usize, u64)> = registry
        .entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.state().tombstone.map(|sequence| (index, sequence)))
        .collect();
    if tombstones.len() < MAX_TOMBSTONES {
        return;
    }
    if let Some((index, _)) = tombstones.iter().min_by_key(|(_, sequence)| *sequence) {
        registry.entries.remove(*index);
    }
}
