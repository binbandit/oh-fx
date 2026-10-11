use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::Mode;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, kill_process_group, setsid, umask, waitpid};
use rustix::termios::{LocalModes, OptionalActions, Winsize, tcgetattr, tcsetattr, tcsetpgrp};

use super::control::{
    COMMAND_RELEASE_BYTE, ControlKind, LAUNCHER_MODE, LAUNCHER_RELEASE_BYTE, LauncherConfig,
    MARKER_ACK, MARKER_FRAME_LEN, MarkerKind, NONCE_LEN, StartupFailure, write_control_frame,
};

const CONTROL_POLL: Duration = Duration::from_millis(50);
const MARKER_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const PRIVATE_FILE_MODE: u32 = 0o600;

pub fn is_terminal_launcher_invocation(args: &[OsString]) -> bool {
    matches!(args, [mode] if mode == LAUNCHER_MODE)
}

pub fn run_terminal_launcher() -> ! {
    std::process::exit(i32::from(launch().is_err()))
}

fn launch() -> io::Result<()> {
    let mut input = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    let config = LauncherConfig::read(&mut input)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let paths = LauncherPaths::new(&config);
    let files = Arc::new(PrivateFiles::default());
    files.write(&paths.bootstrap, &config.bootstrap)?;
    files.write(&paths.command, config.command.as_bytes())?;
    let listener = files.listen(&paths.control)?;
    setsid()?;
    rustix::process::ioctl_tiocsctty(io::stdout())?;
    tcsetwinsize(&config)?;
    write_control_frame(&mut io::stderr(), ControlKind::Prepared, 0)?;
    let mut release = [0; 1];
    input.read_exact(&mut release)?;
    if release[0] != LAUNCHER_RELEASE_BYTE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "InvalidLauncherRelease",
        ));
    }
    let Ok(child) = spawn_shell(&config) else {
        return write_control_frame(
            &mut io::stderr(),
            ControlKind::StartupFailed,
            StartupFailure::ShellUnavailable as u32,
        );
    };
    let shell = Pid::from_child(&child);
    if let Err(error) = tcsetpgrp(io::stdout(), shell) {
        kill_and_reap(shell);
        return Err(error.into());
    }
    let watch = Arc::new(Watch::default());
    let watchdog = {
        let watch = Arc::clone(&watch);
        thread::Builder::new().spawn(move || watch_owner(&input, shell, &watch))
    };
    let watchdog = match watchdog {
        Ok(watchdog) => watchdog,
        Err(error) => {
            kill_and_reap(shell);
            return Err(error);
        }
    };
    let server = ControlServer {
        listener,
        paths,
        files: Arc::clone(&files),
        nonce: config.control_nonce.into_bytes(),
        shell,
        phase: Phase::AwaitingShell,
        frames: io::stderr(),
        terminal: io::stdout().as_fd().try_clone_to_owned().ok(),
    };
    let control = {
        let watch = Arc::clone(&watch);
        thread::Builder::new().spawn(move || server.serve(&watch))
    };
    let control = match control {
        Ok(control) => control,
        Err(error) => {
            watch.finish();
            kill_and_reap(shell);
            let _ = watchdog.join();
            return Err(error);
        }
    };
    let status = wait_shell(shell);
    watch.finish();
    if status.is_err() {
        let _ = kill_process_group(shell, Signal::KILL);
    }
    let _ = watchdog.join();
    let phase = control.join().ok().and_then(Result::ok);
    let status = status?;
    release_terminal();
    let mut control_frames = io::stderr();
    match phase {
        None => write_control_frame(
            &mut control_frames,
            ControlKind::StartupFailed,
            StartupFailure::ControlFailed as u32,
        ),
        Some(Phase::CommandStarted) => {
            let (kind, value) = termination_frame(status);
            write_control_frame(&mut control_frames, kind, value)
        }
        Some(Phase::AwaitingShell | Phase::ShellReady) => write_control_frame(
            &mut control_frames,
            ControlKind::StartupFailed,
            StartupFailure::ProfileFailed as u32,
        ),
    }
}

fn tcsetwinsize(config: &LauncherConfig) -> io::Result<()> {
    rustix::termios::tcsetwinsize(
        io::stdout(),
        Winsize {
            ws_row: config.dimensions.rows,
            ws_col: config.dimensions.columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )?;
    Ok(())
}

fn spawn_shell(config: &LauncherConfig) -> io::Result<std::process::Child> {
    let terminal = || io::stdout().as_fd().try_clone_to_owned().map(Stdio::from);
    let (program, arguments) = config
        .argv
        .split_first()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    Command::new(OsStr::from_bytes(program))
        .args(arguments.iter().map(|argument| OsStr::from_bytes(argument)))
        .stdin(terminal()?)
        .stdout(terminal()?)
        .stderr(terminal()?)
        .current_dir(config.cwd())
        .process_group(0)
        .spawn()
}

fn kill_and_reap(shell: Pid) {
    let _ = kill_process_group(shell, Signal::KILL);
    let _ = wait_shell(shell);
}

fn release_terminal() {
    if let Ok(null) = File::open("/dev/null") {
        let _ = rustix::stdio::dup2_stdout(&null);
    }
}

fn restore_echo(terminal: &OwnedFd) -> io::Result<()> {
    let mut modes = tcgetattr(terminal)?;
    modes.local_modes.insert(LocalModes::ECHO);
    tcsetattr(terminal, OptionalActions::Now, &modes)?;
    Ok(())
}

fn wait_shell(shell: Pid) -> io::Result<ExitStatus> {
    let mut options = WaitOptions::UNTRACED;
    loop {
        let status = match waitpid(Some(shell), options) {
            Err(Errno::INTR) | Ok(None) => continue,
            Err(error) => return Err(error.into()),
            Ok(Some((_, status))) => ExitStatus::from_raw(status.as_raw()),
        };
        match status.stopped_signal() {
            Some(signal) if is_terminal_stop(signal) => {
                kill_process_group(shell, Signal::CONT)?;
            }
            Some(_) => options = WaitOptions::empty(),
            None => return Ok(status),
        }
    }
}

fn is_terminal_stop(signal: i32) -> bool {
    signal == Signal::TTIN.as_raw() || signal == Signal::TTOU.as_raw()
}

fn termination_frame(status: ExitStatus) -> (ControlKind, u32) {
    let exited = status
        .code()
        .and_then(|code| u32::try_from(code).ok())
        .map(|code| (ControlKind::CommandExited, code));
    let signalled = status
        .signal()
        .and_then(|signal| u32::try_from(signal).ok())
        .map(|signal| (ControlKind::CommandSignal, signal));
    exited
        .or(signalled)
        .unwrap_or((ControlKind::InvalidTerm, 0))
}

#[derive(Default)]
struct Watch {
    state: Mutex<WatchState>,
    changed: Condvar,
}

#[derive(Default)]
struct WatchState {
    done: bool,
    command_released: bool,
    owner_gone: bool,
}

impl Watch {
    fn state(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, change: impl FnOnce(&mut WatchState)) {
        change(&mut self.state());
        self.changed.notify_all();
    }

    fn finish(&self) {
        self.update(|state| state.done = true);
    }

    fn done(&self) -> bool {
        self.state().done
    }

    fn wait_for_release(&self) -> io::Result<()> {
        let mut state = self.state();
        loop {
            if state.command_released {
                return Ok(());
            }
            if state.owner_gone || state.done {
                return Err(io::Error::other("LauncherHostClosed"));
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

fn watch_owner(input: &File, shell: Pid, watch: &Watch) {
    let timeout = Timespec::try_from(CONTROL_POLL).unwrap_or_default();
    while !watch.done() {
        let mut fds = [PollFd::new(input, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(Errno::INTR) => continue,
            Ok(_) => {}
            Err(_) => return owner_gone(shell, watch),
        }
        let mut byte = [0; 1];
        match rustix::io::read(input, &mut byte) {
            Ok(1) if byte[0] == COMMAND_RELEASE_BYTE => {
                watch.update(|state| state.command_released = true);
            }
            Ok(1) | Err(Errno::INTR | Errno::AGAIN) => {}
            Ok(_) | Err(_) => return owner_gone(shell, watch),
        }
    }
}

fn owner_gone(shell: Pid, watch: &Watch) {
    let mut done = false;
    watch.update(|state| {
        state.owner_gone = true;
        done = state.done;
    });
    if !done {
        let _ = kill_process_group(shell, Signal::KILL);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitingShell,
    ShellReady,
    CommandStarted,
}

#[derive(Debug, Clone)]
struct LauncherPaths {
    bootstrap: PathBuf,
    command: PathBuf,
    control: PathBuf,
}

impl LauncherPaths {
    fn new(config: &LauncherConfig) -> Self {
        Self {
            bootstrap: PathBuf::from(&config.bootstrap_path),
            command: PathBuf::from(&config.command_path),
            control: PathBuf::from(&config.control_path),
        }
    }
}

#[derive(Debug, Default)]
struct PrivateFiles {
    created: Mutex<Vec<PathBuf>>,
}

impl PrivateFiles {
    fn created(&self) -> MutexGuard<'_, Vec<PathBuf>> {
        self.created.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn track(&self, path: &Path) {
        self.created().push(path.to_path_buf());
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        write_private(path, bytes)?;
        self.track(path);
        Ok(())
    }

    fn listen(&self, path: &Path) -> io::Result<UnixListener> {
        let _ = fs::remove_file(path);
        let previous = umask(Mode::RWXG | Mode::RWXO | Mode::XUSR);
        let bound = UnixListener::bind(path);
        umask(previous);
        let listener = bound?;
        self.track(path);
        listener.set_nonblocking(true)?;
        Ok(listener)
    }

    fn remove(&self, path: &Path) {
        let mut created = self.created();
        if let Some(index) = created.iter().position(|candidate| candidate == path) {
            created.swap_remove(index);
            let _ = fs::remove_file(path);
        }
    }
}

impl Drop for PrivateFiles {
    fn drop(&mut self) {
        for path in self.created().drain(..) {
            let _ = fs::remove_file(path);
        }
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)?;
    file.write_all(bytes).inspect_err(|_| {
        let _ = fs::remove_file(path);
    })
}

struct ControlServer<W> {
    listener: UnixListener,
    paths: LauncherPaths,
    files: Arc<PrivateFiles>,
    nonce: Vec<u8>,
    shell: Pid,
    phase: Phase,
    frames: W,
    terminal: Option<OwnedFd>,
}

impl<W: Write> ControlServer<W> {
    fn serve(mut self, watch: &Watch) -> io::Result<Phase> {
        let timeout = Timespec::try_from(CONTROL_POLL).unwrap_or_default();
        while !watch.done() {
            let mut fds = [PollFd::new(&self.listener, PollFlags::IN)];
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) | Err(Errno::INTR) => continue,
                Ok(_) => {}
                Err(error) => return Err(error.into()),
            }
            let mut stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut frame = [0; MARKER_FRAME_LEN];
            let received = stream
                .set_nonblocking(false)
                .and_then(|()| stream.set_read_timeout(Some(MARKER_FRAME_TIMEOUT)))
                .and_then(|()| stream.read_exact(&mut frame));
            let Some(kind) = received.ok().and_then(|()| self.marker(&frame)) else {
                continue;
            };
            if !self.accept(kind, watch)? {
                continue;
            }
            let finished = self.phase == Phase::CommandStarted;
            if finished {
                self.files.remove(&self.paths.control);
            }
            stream.write_all(&[MARKER_ACK])?;
            if finished {
                break;
            }
        }
        Ok(self.phase)
    }

    fn marker(&self, frame: &[u8; MARKER_FRAME_LEN]) -> Option<MarkerKind> {
        let (nonce, kind) = frame.split_at(NONCE_LEN);
        if nonce != self.nonce.as_slice() {
            return None;
        }
        MarkerKind::from_byte(kind[0])
    }

    fn accept(&mut self, kind: MarkerKind, watch: &Watch) -> io::Result<bool> {
        let shell = u32::try_from(self.shell.as_raw_pid()).unwrap_or_default();
        match (kind, self.phase) {
            (MarkerKind::ShellReady, Phase::AwaitingShell) => {
                if let Some(terminal) = &self.terminal {
                    let _ = restore_echo(terminal);
                }
                self.files.remove(&self.paths.bootstrap);
                write_control_frame(&mut self.frames, ControlKind::ShellReady, shell)?;
                self.phase = Phase::ShellReady;
            }
            (MarkerKind::CommandStarted, Phase::ShellReady) => {
                self.files.remove(&self.paths.command);
                write_control_frame(&mut self.frames, ControlKind::CommandStarted, shell)?;
                watch.wait_for_release()?;
                self.phase = Phase::CommandStarted;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
