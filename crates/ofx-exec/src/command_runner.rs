mod foreground_session;
mod launch_probe;
mod natural_drain;
mod status_probe;
#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, kill_process, kill_process_group, waitid,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::{oneshot, watch};
use tokio::time::{Instant, sleep_until};

pub use foreground_session::{is_foreground_session_invocation, run_foreground_session};

use crate::command_contract::CommandStatus;
use foreground_session::{
    FORCE_SIGNAL, LAUNCH_FAILURE_EXIT_CODE, LAUNCH_FAILURE_PREFIX, NO_DEADLINE, NONCE_HEX_BYTES,
    READY_BYTE, RELEASE_BYTE, STATUS_PREFIX, TOKEN,
};
use launch_probe::LaunchProbe;
use natural_drain::NaturalDrain;
use status_probe::StatusProbe;

const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const TERMINATION_GRACE: Duration = Duration::from_millis(700);
pub(crate) const TERMINATION_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const SUPERVISOR_HANDOFF: Duration = Duration::from_millis(200);
const READ_CHUNK_BYTES: usize = 4096;
const NONCE_BYTES: usize = NONCE_HEX_BYTES / 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSupervisor {
    executable: PathBuf,
}

impl SessionSupervisor {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn current_executable() -> io::Result<Self> {
        if cfg!(target_os = "linux") {
            return Ok(Self::new("/proc/self/exe"));
        }
        std::env::current_exe().map(Self::new)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StopIntent {
    Graceful,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

pub(crate) struct CapturedCommand<'a> {
    pub(crate) argv: &'a [OsString],
    pub(crate) cwd: &'a Path,
    pub(crate) deadline: Option<Instant>,
    pub(crate) supervisor: &'a SessionSupervisor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapturedOutcome {
    pub(crate) status: CommandStatus,
    pub(crate) output_incomplete: bool,
    pub(crate) duration: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunError {
    TimeoutExpired,
    CancelledBeforeExecution,
    Failed(&'static str),
}

impl RunError {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::TimeoutExpired => "TimeoutExpired",
            Self::CancelledBeforeExecution => "CancelledBeforeExecution",
            Self::Failed(name) => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalScope {
    ProcessGroup,
    Supervisor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Natural,
    Cancelled,
    TimedOut,
}

struct Termination {
    source: Source,
    intent: StopIntent,
    started: Instant,
    forced: Option<Instant>,
}

pub(crate) async fn run_captured(
    command: CapturedCommand<'_>,
    stop: &mut watch::Receiver<Option<StopIntent>>,
    sink: &mut (dyn FnMut(OutputStream, &[u8]) + Send),
) -> Result<CapturedOutcome, RunError> {
    if stop.borrow_and_update().is_some() {
        return Err(RunError::CancelledBeforeExecution);
    }
    if command
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Err(RunError::TimeoutExpired);
    }
    let started = Instant::now();
    let nonce = nonce()?;
    let mut child = spawn_supervisor(&command)?;
    let (Some(mut input), Some(stdout), Some(mut stderr), Some(group)) = (
        child.stdin.take(),
        child.stdout.take(),
        child.stderr.take(),
        child_pid(&child),
    ) else {
        return Err(abandon(&mut child, RunError::Failed("SpawnFailed")).await);
    };
    if let Err(error) = await_ready(&mut stderr, stop, command.deadline).await {
        return Err(abandon(&mut child, error).await);
    }
    let exited = match watch_exit(group) {
        Ok(exited) => exited,
        Err(error) => return Err(abandon(&mut child, RunError::Failed(error_name(&error))).await),
    };
    let mut control = nonce.clone().into_bytes();
    control.push(RELEASE_BYTE);
    if let Err(error) = input.write_all(&control).await {
        return Err(abandon(&mut child, RunError::Failed(error_name(&error))).await);
    }
    let mut collection = Collection::new(
        &mut child,
        group,
        (stdout, stderr),
        &nonce,
        command.deadline,
        exited,
    );
    let exit = collection.collect(stop, sink).await;
    drop(input);
    let outcome = collection.finish(exit, started);
    if exit.is_none() {
        reap_in_background(child);
    }
    outcome
}

fn reap_in_background(mut child: Child) {
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
}

fn spawn_supervisor(command: &CapturedCommand<'_>) -> Result<Child, RunError> {
    let deadline = command.deadline.map_or_else(
        || NO_DEADLINE.to_owned(),
        |deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now()) + SUPERVISOR_HANDOFF;
            remaining.as_millis().to_string()
        },
    );
    tokio::process::Command::new(&command.supervisor.executable)
        .arg(TOKEN)
        .arg(deadline)
        .args(command.argv)
        .current_dir(command.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| RunError::Failed(error_name(&error)))
}

async fn await_ready(
    stderr: &mut ChildStderr,
    stop: &mut watch::Receiver<Option<StopIntent>>,
    deadline: Option<Instant>,
) -> Result<(), RunError> {
    let setup_deadline = Instant::now() + SETUP_TIMEOUT;
    let deadline_or_far = deadline.unwrap_or(setup_deadline);
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || stop.borrow_and_update().is_some() {
                    return Err(RunError::CancelledBeforeExecution);
                }
            }
            () = sleep_until(deadline_or_far), if deadline.is_some() => {
                return Err(RunError::TimeoutExpired);
            }
            () = sleep_until(setup_deadline) => {
                return Err(RunError::Failed("ForegroundSessionSetupTimedOut"));
            }
            byte = stderr.read_u8() => {
                return match byte {
                    Ok(READY_BYTE) => Ok(()),
                    Ok(_) => Err(RunError::Failed("InvalidForegroundSessionReady")),
                    Err(_) => Err(RunError::Failed("ForegroundSessionSetupFailed")),
                };
            }
        }
    }
}

async fn abandon(child: &mut Child, error: RunError) -> RunError {
    if let Some(group) = child_pid(child) {
        let _ = kill_process_group(group, Signal::KILL);
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    error
}

fn child_pid(child: &Child) -> Option<Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
}

fn watch_exit(supervisor: Pid) -> io::Result<oneshot::Receiver<()>> {
    let (exited, receiver) = oneshot::channel();
    thread::Builder::new().spawn(move || {
        while matches!(
            waitid(
                WaitId::Pid(supervisor),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ),
            Err(Errno::INTR)
        ) {}
        let _ = exited.send(());
    })?;
    Ok(receiver)
}

fn nonce() -> Result<String, RunError> {
    let mut bytes = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| RunError::Failed("SystemResources"))?;
    Ok(bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    }))
}

struct Collection<'a> {
    exited: oneshot::Receiver<()>,
    child: &'a mut Child,
    group: Pid,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    launch: LaunchProbe,
    status: StatusProbe,
    deadline: Option<Instant>,
    termination: Option<Termination>,
    exit: Option<(ExitStatus, Instant)>,
    drain: Option<NaturalDrain>,
    settled: bool,
    output_incomplete: bool,
    indeterminate: bool,
}

enum Event {
    Read(OutputStream, io::Result<usize>),
    Exited,
    StopRequested(Option<StopIntent>),
    StopClosed,
    Timer,
}

impl<'a> Collection<'a> {
    fn new(
        child: &'a mut Child,
        group: Pid,
        (stdout, stderr): (ChildStdout, ChildStderr),
        nonce: &str,
        deadline: Option<Instant>,
        exited: oneshot::Receiver<()>,
    ) -> Self {
        Self {
            exited,
            child,
            group,
            stdout: Some(stdout),
            stderr: Some(stderr),
            launch: LaunchProbe::new(nonce),
            status: StatusProbe::new(nonce),
            deadline,
            termination: None,
            exit: None,
            drain: None,
            settled: false,
            output_incomplete: false,
            indeterminate: false,
        }
    }

    async fn collect(
        &mut self,
        stop: &mut watch::Receiver<Option<StopIntent>>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) + Send),
    ) -> Option<ExitStatus> {
        let mut stdout_buffer = vec![0; READ_CHUNK_BYTES];
        let mut stderr_buffer = vec![0; READ_CHUNK_BYTES];
        let mut stop_open = true;
        loop {
            if self.stdout.is_none() && self.stderr.is_none() {
                if let Some((exit, _)) = self.exit {
                    return Some(exit);
                }
                if self.settled {
                    return None;
                }
            }
            let waiting_since = Instant::now();
            if self
                .drain
                .is_some_and(|drain| drain.remaining_wait().is_none())
            {
                self.close_streams(sink);
                continue;
            }
            let timer = self.next_timer(waiting_since);
            let running = self.exit.is_none();
            let event = tokio::select! {
                read = read_chunk(&mut self.stdout, &mut stdout_buffer), if self.stdout.is_some() => {
                    Event::Read(OutputStream::Stdout, read)
                }
                read = read_chunk(&mut self.stderr, &mut stderr_buffer), if self.stderr.is_some() => {
                    Event::Read(OutputStream::Stderr, read)
                }
                _ = &mut self.exited, if running => Event::Exited,
                changed = stop.changed(), if running && stop_open => match changed {
                    Ok(()) => Event::StopRequested(*stop.borrow_and_update()),
                    Err(_) => Event::StopClosed,
                },
                () = sleep_until(timer.unwrap_or(waiting_since)), if timer.is_some() => Event::Timer,
            };
            let waited = waiting_since.elapsed();
            match event {
                Event::Read(stream, read) => {
                    let length = self.accept_read(&read, stream);
                    if let Some(drain) = &mut self.drain {
                        drain.record(length.unwrap_or(0), waited);
                    }
                    match (stream, length) {
                        (OutputStream::Stdout, Some(length)) => {
                            sink(stream, &stdout_buffer[..length]);
                        }
                        (OutputStream::Stdout, None) => {}
                        (OutputStream::Stderr, length) => {
                            let emitted = match length {
                                Some(length) => self.filter_stderr(&stderr_buffer[..length]),
                                None => self.flush_stderr(),
                            };
                            if !emitted.is_empty() {
                                sink(stream, &emitted);
                            }
                        }
                    }
                }
                Event::Exited => self.observe_exit().await,
                Event::StopClosed => stop_open = false,
                Event::StopRequested(Some(intent)) => self.request(Source::Cancelled, intent),
                Event::StopRequested(None) => {}
                Event::Timer => {
                    if let Some(drain) = &mut self.drain {
                        drain.record(0, waited);
                    } else if self.on_timer() {
                        self.close_streams(sink);
                    }
                }
            }
        }
    }

    async fn observe_exit(&mut self) {
        let _ = kill_process_group(self.group, Signal::KILL);
        let now = Instant::now();
        let exit = self.child.wait().await.unwrap_or_else(|_| {
            self.indeterminate = true;
            ExitStatus::from_raw(0)
        });
        self.exit = Some((exit, now));
        self.drain = Some(NaturalDrain::default());
    }

    fn close_streams(&mut self, sink: &mut (dyn FnMut(OutputStream, &[u8]) + Send)) {
        let held = self.flush_stderr();
        if !held.is_empty() {
            sink(OutputStream::Stderr, &held);
        }
        self.stdout = None;
        self.stderr = None;
    }

    fn filter_stderr(&mut self, bytes: &[u8]) -> Vec<u8> {
        let passed = self.launch.feed(bytes);
        self.status.feed(&passed)
    }

    fn flush_stderr(&mut self) -> Vec<u8> {
        let released = self.launch.flush();
        let mut emitted = self.status.feed(&released);
        emitted.extend_from_slice(&self.status.flush());
        emitted
    }

    fn accept_read(&mut self, read: &io::Result<usize>, stream: OutputStream) -> Option<usize> {
        match *read {
            Ok(0) => {}
            Ok(length) => return Some(length),
            Err(_) => self.output_incomplete = true,
        }
        match stream {
            OutputStream::Stdout => self.stdout = None,
            OutputStream::Stderr => self.stderr = None,
        }
        None
    }

    fn next_timer(&self, now: Instant) -> Option<Instant> {
        if let Some(drain) = self.drain {
            return drain.remaining_wait().map(|remaining| now + remaining);
        }
        if self.settled {
            return None;
        }
        match &self.termination {
            None => self.deadline,
            Some(Termination {
                started,
                forced: Some(_),
                ..
            }) => Some(*started + TERMINATION_SETTLE_TIMEOUT),
            Some(Termination {
                started,
                forced: None,
                ..
            }) => Some(*started + TERMINATION_GRACE),
        }
    }

    fn on_timer(&mut self) -> bool {
        match &self.termination {
            None => {
                self.request(Source::TimedOut, StopIntent::Force);
                false
            }
            Some(Termination { forced: None, .. }) => {
                self.escalate();
                false
            }
            Some(Termination {
                forced: Some(_), ..
            }) => {
                self.settled = true;
                self.indeterminate = true;
                self.output_incomplete = true;
                self.kill_group();
                true
            }
        }
    }

    fn request(&mut self, source: Source, intent: StopIntent) {
        match &mut self.termination {
            None => {
                self.termination = Some(Termination {
                    source,
                    intent,
                    started: Instant::now(),
                    forced: None,
                });
                match intent {
                    StopIntent::Graceful => self.deliver(StopIntent::Graceful),
                    StopIntent::Force => self.escalate(),
                }
            }
            Some(termination) if intent > termination.intent => {
                termination.intent = intent;
                if termination.forced.is_none() {
                    self.escalate();
                }
            }
            Some(_) => {}
        }
    }

    fn escalate(&mut self) {
        if let Some(termination) = &mut self.termination {
            termination.forced.get_or_insert_with(Instant::now);
        }
        self.deliver(StopIntent::Force);
    }

    fn deliver(&self, intent: StopIntent) {
        if self.exit.is_some() {
            return;
        }
        let _ = match termination_signal(intent) {
            (SignalScope::ProcessGroup, signal) => kill_process_group(self.group, signal),
            (SignalScope::Supervisor, signal) => kill_process(self.group, signal),
        };
    }

    fn kill_group(&self) {
        if self.exit.is_none() {
            let _ = kill_process_group(self.group, Signal::KILL);
        }
    }

    fn finish(
        self,
        exit: Option<ExitStatus>,
        started: Instant,
    ) -> Result<CapturedOutcome, RunError> {
        if exit.and_then(|exit| exit.code()) == Some(LAUNCH_FAILURE_EXIT_CODE)
            && let Some(name) = self.launch.launch_failure()
        {
            return Err(RunError::Failed(name));
        }
        let exited_at = self.exit.map_or_else(Instant::now, |(_, at)| at);
        let source = match &self.termination {
            Some(termination) => termination.source,
            None if self.deadline.is_some_and(|deadline| exited_at >= deadline) => Source::TimedOut,
            None => Source::Natural,
        };
        if source == Source::TimedOut {
            return Err(RunError::TimeoutExpired);
        }
        let status = match (self.indeterminate, self.status.reported(), exit) {
            (false, Some(reported), _) => reported,
            (false, None, Some(exit)) => supervisor_status(exit),
            _ => CommandStatus::Indeterminate,
        };
        Ok(CapturedOutcome {
            status,
            output_incomplete: self.output_incomplete || self.indeterminate,
            duration: started.elapsed(),
        })
    }
}

fn termination_signal(intent: StopIntent) -> (SignalScope, Signal) {
    match intent {
        StopIntent::Graceful => (SignalScope::ProcessGroup, Signal::TERM),
        StopIntent::Force => (SignalScope::Supervisor, FORCE_SIGNAL),
    }
}

fn supervisor_status(exit: ExitStatus) -> CommandStatus {
    match (exit.code(), exit.signal()) {
        (Some(code), _) => CommandStatus::ExitCode(code.into()),
        (None, Some(signal)) => {
            u32::try_from(signal).map_or(CommandStatus::Indeterminate, CommandStatus::Signal)
        }
        (None, None) => CommandStatus::Indeterminate,
    }
}

async fn read_chunk<R: AsyncRead + Unpin>(
    reader: &mut Option<R>,
    buffer: &mut [u8],
) -> io::Result<usize> {
    match reader {
        Some(reader) => reader.read(buffer).await,
        None => Ok(0),
    }
}

pub(crate) fn error_name(error: &io::Error) -> &'static str {
    let Some(code) = error.raw_os_error() else {
        return match error.kind() {
            io::ErrorKind::NotFound => "FileNotFound",
            io::ErrorKind::PermissionDenied => "AccessDenied",
            io::ErrorKind::BrokenPipe => "BrokenPipe",
            io::ErrorKind::OutOfMemory => "OutOfMemory",
            _ => "Unexpected",
        };
    };
    match Errno::from_raw_os_error(code) {
        Errno::TOOBIG | Errno::NOMEM | Errno::AGAIN => "SystemResources",
        Errno::ACCESS => "AccessDenied",
        Errno::PERM => "PermissionDenied",
        Errno::NOEXEC | Errno::INVAL => "InvalidExe",
        Errno::IO | Errno::LOOP => "FileSystem",
        Errno::ISDIR => "IsDir",
        Errno::NOENT => "FileNotFound",
        Errno::NOTDIR => "NotDir",
        Errno::TXTBSY => "FileBusy",
        Errno::MFILE => "ProcessFdQuotaExceeded",
        Errno::NFILE => "SystemFdQuotaExceeded",
        Errno::NAMETOOLONG => "NameTooLong",
        Errno::PIPE => "BrokenPipe",
        _ => "Unexpected",
    }
}

fn launch_failure_prefix(nonce: &str) -> Vec<u8> {
    framed_prefix(LAUNCH_FAILURE_PREFIX, nonce)
}

fn status_prefix(nonce: &str) -> Vec<u8> {
    framed_prefix(STATUS_PREFIX, nonce)
}

fn framed_prefix(marker: &[u8], nonce: &str) -> Vec<u8> {
    let mut prefix = marker.to_vec();
    prefix.extend_from_slice(nonce.as_bytes());
    prefix.push(b':');
    prefix
}
