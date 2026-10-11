use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ofx_exec::{
    ControlFrame, ControlKind, Dimensions, LaunchError, LaunchedTerminal, Liveness, StartupFailure,
    TerminalLaunch, is_terminal_control_invocation, is_terminal_launcher_invocation,
    launch_terminal, read_control_frame, run_terminal_control, run_terminal_launcher,
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process, kill_process_group, test_kill_process};

const BASH: &str = "/bin/bash";
const LONG: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);
const SCENARIO_VARIABLE: &str = "OH_FX_TEST_TERMINAL_SCENARIO";
const SOCKET_MODE_VARIABLE: &str = "OH_FX_TEST_TERMINAL_SOCKET_MODES";
const SIGKILL: u32 = 9;

const TESTS: &[(&str, fn())] = &[
    (
        "a_command_runs_in_its_terminal_and_reports_its_exit",
        a_command_runs_in_its_terminal_and_reports_its_exit,
    ),
    (
        "the_command_is_the_foreground_job_of_its_controlling_terminal",
        the_command_is_the_foreground_job_of_its_controlling_terminal,
    ),
    (
        "a_command_killed_by_a_signal_reports_the_signal",
        a_command_killed_by_a_signal_reports_the_signal,
    ),
    (
        "the_owner_leaving_kills_the_shell_process_group",
        the_owner_leaving_kills_the_shell_process_group,
    ),
    (
        "a_missing_shell_fails_to_start",
        a_missing_shell_fails_to_start,
    ),
    (
        "a_shell_that_exits_during_startup_fails_its_profile",
        a_shell_that_exits_during_startup_fails_its_profile,
    ),
    (
        "a_launcher_that_never_prepares_fails_the_launch",
        a_launcher_that_never_prepares_fails_the_launch,
    ),
    (
        "the_control_socket_is_private_to_its_owner",
        the_control_socket_is_private_to_its_owner,
    ),
];

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if is_terminal_control_invocation(&args) {
        record_control_socket_mode(&args);
        run_terminal_control(&args);
    }
    if is_terminal_launcher_invocation(&args) {
        run_terminal_launcher();
    }
    if let Some(scenario) = env::var_os(SCENARIO_VARIABLE) {
        return run_scenario(&scenario);
    }
    let filters: Vec<String> = args
        .iter()
        .filter_map(|argument| argument.to_str())
        .filter(|argument| !argument.starts_with('-'))
        .map(str::to_owned)
        .collect();
    let mut failed = Vec::new();
    for (name, test) in TESTS {
        if !filters.is_empty() && !filters.iter().any(|filter| name.contains(filter.as_str())) {
            continue;
        }
        let outcome = panic::catch_unwind(test);
        println!(
            "test {name} ... {}",
            if outcome.is_ok() { "ok" } else { "FAILED" }
        );
        if outcome.is_err() {
            failed.push(name);
        }
    }
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        println!("failed: {failed:?}");
        ExitCode::FAILURE
    }
}

struct Terminal {
    liveness: Option<Liveness>,
    launcher: Option<Pid>,
    exited: Receiver<io::Result<ExitStatus>>,
    frames: Receiver<io::Result<ControlFrame>>,
    shell: Option<Pid>,
    ended: bool,
    output: Arc<Output>,
}

#[derive(Default)]
struct Output {
    bytes: Mutex<Vec<u8>>,
    changed: Condvar,
}

impl Output {
    fn bytes(&self) -> MutexGuard<'_, Vec<u8>> {
        self.bytes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn drain(&self, master: &OwnedFd) {
        let timeout = Timespec::try_from(POLL).unwrap_or_default();
        let mut chunk = [0; 4096];
        loop {
            let mut fds = [PollFd::new(master, PollFlags::IN)];
            match poll(&mut fds, Some(&timeout)) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(_) => return,
            }
            match rustix::io::read(master, &mut chunk) {
                Ok(0) => return,
                Ok(count) => {
                    self.bytes().extend_from_slice(&chunk[..count]);
                    self.changed.notify_all();
                }
                Err(Errno::AGAIN | Errno::INTR) => {}
                Err(_) => return,
            }
        }
    }
}

impl Terminal {
    fn start(command: &str) -> Self {
        Self::launch(Path::new(BASH), true, &env::temp_dir(), command).expect("launch a terminal")
    }

    fn launch(
        shell: &Path,
        clean_start: bool,
        cwd: &Path,
        command: &str,
    ) -> Result<Self, LaunchError> {
        Self::launch_with(&executable(), shell, clean_start, cwd, command)
    }

    fn launch_with(
        executable: &Path,
        shell: &Path,
        clean_start: bool,
        cwd: &Path,
        command: &str,
    ) -> Result<Self, LaunchError> {
        let LaunchedTerminal {
            master,
            mut control,
            mut liveness,
            mut launcher,
        } = launch_terminal(&TerminalLaunch {
            executable,
            shell,
            clean_start,
            cwd,
            command,
            dimensions: Dimensions {
                rows: 24,
                columns: 80,
            },
        })?;
        let output = Arc::new(Output::default());
        let drained = Arc::clone(&output);
        thread::spawn(move || drained.drain(&master));
        let (sender, frames) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let frame = read_control_frame(&mut control);
                let ended = frame.is_err();
                if sender.send(frame).is_err() || ended {
                    break;
                }
            }
        });
        let launcher_pid = i32::try_from(launcher.id()).ok().and_then(Pid::from_raw);
        let (status_sender, exited) = mpsc::channel();
        thread::spawn(move || {
            let _ = status_sender.send(launcher.wait());
        });
        liveness.release_launcher().expect("release the launcher");
        Ok(Self {
            liveness: Some(liveness),
            launcher: launcher_pid,
            exited,
            frames,
            shell: None,
            ended: false,
            output,
        })
    }

    fn frame(&mut self) -> ControlFrame {
        let frame = self
            .frames
            .recv_timeout(LONG)
            .expect("the launcher reports a frame in time")
            .expect("the launcher reports a valid frame");
        match frame.kind {
            ControlKind::ShellReady | ControlKind::CommandStarted => {
                self.shell = i32::try_from(frame.value).ok().and_then(Pid::from_raw);
            }
            ControlKind::CommandExited
            | ControlKind::CommandSignal
            | ControlKind::StartupFailed
            | ControlKind::InvalidTerm => self.ended = true,
            ControlKind::Prepared => {}
        }
        frame
    }

    fn expect(&mut self, kind: ControlKind) -> u32 {
        let frame = self.frame();
        assert_eq!(frame.kind, kind, "{frame:?}");
        frame.value
    }

    fn start_command(&mut self) -> u32 {
        let shell = self.expect(ControlKind::ShellReady);
        assert_eq!(self.expect(ControlKind::CommandStarted), shell);
        self.liveness
            .as_mut()
            .expect("the terminal is still owned")
            .release_command()
            .expect("release the command");
        shell
    }

    fn output_containing(&self, needle: &str) -> String {
        self.output_until(needle, |text| text.contains(needle))
    }

    fn output_until(&self, expected: &str, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + LONG;
        let mut bytes = self.output.bytes();
        loop {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if done(&text) {
                return text;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "{expected:?} never appeared in {text:?}"
            );
            bytes = self
                .output
                .changed
                .wait_timeout(bytes, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn launcher_exit(&mut self) -> ExitStatus {
        let status = self
            .exited
            .recv_timeout(LONG)
            .expect("the launcher exits in time")
            .expect("wait for the launcher");
        self.launcher = None;
        status
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if !self.ended
            && let Some(shell) = self.shell
        {
            let _ = kill_process_group(shell, Signal::KILL);
        }
        if let Some(launcher) = self.launcher.take() {
            let _ = kill_process(launcher, Signal::KILL);
        }
    }
}

fn alive(pid: Pid) -> bool {
    test_kill_process(pid).is_ok()
}

fn gone_in_time(pid: Pid) -> bool {
    let deadline = Instant::now() + LONG;
    while alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
    true
}

fn executable() -> PathBuf {
    env::current_exe().expect("the test executable")
}

fn canonical_temp_dir() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("create a directory");
    let path = fs::canonicalize(directory.path()).expect("canonicalize the directory");
    (directory, path)
}

fn a_command_runs_in_its_terminal_and_reports_its_exit() {
    let (_directory, cwd) = canonical_temp_dir();
    let mut terminal = Terminal::launch(
        Path::new(BASH),
        true,
        &cwd,
        "printf 'size %s\\n' \"$(stty size)\"; printf 'cwd %s\\n' \"$PWD\"; exit 7",
    )
    .expect("launch a terminal");
    terminal.start_command();
    assert_eq!(terminal.expect(ControlKind::CommandExited), 7);
    terminal.output_containing("size 24 80");
    terminal.output_containing(&format!("cwd {}", cwd.display()));
    assert!(terminal.launcher_exit().success());
}

fn the_command_is_the_foreground_job_of_its_controlling_terminal() {
    let mut terminal = Terminal::start(
        "tty >/dev/null && [ \"$(ps -o tpgid= -p $$ | tr -d ' ')\" = \"$$\" ] && echo foreground-job",
    );
    terminal.start_command();
    assert_eq!(terminal.expect(ControlKind::CommandExited), 0);
    terminal.output_containing("foreground-job");
}

fn a_command_killed_by_a_signal_reports_the_signal() {
    let mut terminal = Terminal::start("kill -KILL $$");
    terminal.start_command();
    assert_eq!(terminal.expect(ControlKind::CommandSignal), SIGKILL);
    assert!(terminal.launcher_exit().success());
}

fn the_owner_leaving_kills_the_shell_process_group() {
    let mut terminal = Terminal::start("set +m; sleep 30 & echo \"child $!\"; wait");
    terminal.start_command();
    let text = terminal.output_until("a child pid line", |text| {
        text.split_once("child ")
            .is_some_and(|(_, rest)| rest.contains('\n'))
    });
    let child = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("child "))
        .and_then(|pid| pid.parse().ok())
        .and_then(Pid::from_raw)
        .expect("the background child's pid");
    assert!(alive(child));
    terminal.liveness = None;
    assert_eq!(terminal.expect(ControlKind::CommandSignal), SIGKILL);
    assert!(terminal.launcher_exit().success());
    let killed = gone_in_time(child);
    if !killed {
        let _ = kill_process(child, Signal::KILL);
    }
    assert!(killed, "the background child outlived its owner");
}

fn a_missing_shell_fails_to_start() {
    let mut terminal = Terminal::launch(
        Path::new("/nonexistent/bash"),
        true,
        &env::temp_dir(),
        "true",
    )
    .expect("launch a terminal");
    assert_eq!(
        terminal.expect(ControlKind::StartupFailed),
        StartupFailure::ShellUnavailable as u32
    );
    assert!(terminal.launcher_exit().success());
}

fn scenario(name: &str, home: &Path, environment: &[(&str, &Path)]) -> String {
    let (sender, reported) = mpsc::channel();
    let mut scenario = Command::new(executable())
        .env(SCENARIO_VARIABLE, name)
        .env("HOME", home)
        .envs(environment.iter().copied())
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("run the scenario");
    let mut stdout = scenario.stdout.take().expect("the scenario's output");
    thread::spawn(move || {
        let mut text = String::new();
        let _ = io::Read::read_to_string(&mut stdout, &mut text);
        let _ = sender.send(text);
    });
    let report = reported.recv_timeout(LONG);
    let _ = scenario.kill();
    let _ = scenario.wait();
    report.expect("the scenario reports in time")
}

fn run_scenario(name: &OsString) -> ExitCode {
    let login = name == "profile";
    let command = if login { "true" } else { "exit 0" };
    let mut terminal = Terminal::launch(Path::new(BASH), !login, &env::temp_dir(), command)
        .expect("launch a terminal");
    let frame = loop {
        let frame = terminal.frame();
        match frame.kind {
            ControlKind::ShellReady => {}
            ControlKind::CommandStarted => terminal
                .liveness
                .as_mut()
                .expect("the terminal is still owned")
                .release_command()
                .expect("release the command"),
            _ => break frame,
        }
    };
    let mut stdout = io::stdout();
    let _ = writeln!(stdout, "{:?} {}", frame.kind, frame.value);
    ExitCode::SUCCESS
}

fn record_control_socket_mode(args: &[OsString]) {
    let (Some(record), Some(socket)) = (env::var_os(SOCKET_MODE_VARIABLE), args.get(1)) else {
        return;
    };
    let Ok(metadata) = fs::symlink_metadata(socket) else {
        return;
    };
    let kind = if metadata.file_type().is_socket() {
        "socket"
    } else {
        "other"
    };
    let line = format!("{kind} {:o}\n", metadata.permissions().mode() & 0o7777);
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(record)
        .and_then(|mut file| file.write_all(line.as_bytes()));
}

fn a_shell_that_exits_during_startup_fails_its_profile() {
    let (_home, home) = canonical_temp_dir();
    fs::write(home.join(".bash_profile"), "exit 9\n").expect("write a startup file");
    assert_eq!(
        scenario("profile", &home, &[]),
        format!(
            "{:?} {}\n",
            ControlKind::StartupFailed,
            StartupFailure::ProfileFailed as u32
        )
    );
}

fn the_control_socket_is_private_to_its_owner() {
    let (_home, home) = canonical_temp_dir();
    let modes = home.join("modes");
    assert_eq!(
        scenario("socket", &home, &[(SOCKET_MODE_VARIABLE, &modes)]),
        format!("{:?} 0\n", ControlKind::CommandExited)
    );
    assert_eq!(
        fs::read_to_string(&modes).expect("the recorded socket modes"),
        "socket 600\nsocket 600\n"
    );
}

fn a_launcher_that_never_prepares_fails_the_launch() {
    let true_program = ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.exists())
        .expect("a true program");
    let launched = Terminal::launch_with(
        true_program,
        Path::new(BASH),
        true,
        &env::temp_dir(),
        "true",
    );
    assert_eq!(
        launched.err(),
        Some(LaunchError::LauncherNotPrepared),
        "a launch through a program that is not a launcher"
    );
}
