use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::panic;
use std::path::Path;
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use ofx_exec::{
    is_terminal_control_invocation, is_terminal_launcher_invocation, run_terminal_control,
    run_terminal_launcher,
};
use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use rustix::termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize};
use serde_json::json;

const LONG: Duration = Duration::from_secs(20);
const NONCE: &str = "0123456789abcdef0123456789abcdef";
const LAUNCHER_MODE: &str = "--oh-fx-internal-terminal-launcher";
const CONTROL_MODE: &str = "--oh-fx-internal-terminal-control";
const SHELL_READY: u8 = 2;
const COMMAND_STARTED: u8 = 3;
const COMMAND_EXITED: u8 = 4;

const TESTS: &[(&str, fn())] = &[(
    "shell_ready_turns_echo_back_on_after_startup_turned_it_off",
    shell_ready_turns_echo_back_on_after_startup_turned_it_off,
)];

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if is_terminal_control_invocation(&args) {
        run_terminal_control(&args);
    }
    if is_terminal_launcher_invocation(&args) {
        run_terminal_launcher();
    }
    let mut failed = Vec::new();
    for (name, test) in TESTS {
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

struct Launch {
    master: OwnedFd,
    liveness: Option<std::process::ChildStdin>,
    frames: Receiver<[u8; 5]>,
    exited: Receiver<io::Result<ExitStatus>>,
    output: Receiver<Vec<u8>>,
    launcher: Option<Pid>,
    shell: Option<Pid>,
    ended: bool,
}

impl Launch {
    fn start(directory: &Path, bootstrap: &str) -> Self {
        let (master, slave) = open_terminal();
        let mut launcher = Command::new(env::current_exe().expect("the test executable"))
            .arg(LAUNCHER_MODE)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(slave))
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the launcher");
        let mut liveness = launcher.stdin.take().expect("the launcher's input");
        let mut control = launcher
            .stderr
            .take()
            .expect("the launcher's control frames");
        let request = launcher_request(directory, bootstrap);
        liveness
            .write_all(&request)
            .expect("configure the launcher");
        let (frame_sender, frames) = mpsc::channel();
        thread::spawn(move || {
            let mut frame = [0; 5];
            while control.read_exact(&mut frame).is_ok() {
                if frame_sender.send(frame).is_err() {
                    break;
                }
            }
        });
        let reader = master.try_clone().expect("a terminal reader");
        let (output_sender, output) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = File::from(reader).read_to_end(&mut bytes);
            let _ = output_sender.send(bytes);
        });
        let pid = i32::try_from(launcher.id()).ok().and_then(Pid::from_raw);
        let (status_sender, exited) = mpsc::channel();
        thread::spawn(move || {
            let mut launcher: Child = launcher;
            let _ = status_sender.send(launcher.wait());
        });
        let mut launch = Self {
            master,
            liveness: Some(liveness),
            frames,
            exited,
            output,
            launcher: pid,
            shell: None,
            ended: false,
        };
        assert_eq!(launch.frame(), (1, 0), "the launcher prepares");
        launch.send(1);
        launch
    }

    fn frame(&mut self) -> (u8, u32) {
        let frame = self
            .frames
            .recv_timeout(LONG)
            .expect("a control frame in time");
        let value = u32::from_le_bytes([frame[1], frame[2], frame[3], frame[4]]);
        match frame[0] {
            SHELL_READY | COMMAND_STARTED => {
                self.shell = i32::try_from(value).ok().and_then(Pid::from_raw);
            }
            1 => {}
            _ => self.ended = true,
        }
        (frame[0], value)
    }

    fn send(&mut self, byte: u8) {
        self.liveness
            .as_mut()
            .expect("the launcher is still owned")
            .write_all(&[byte])
            .expect("write to the launcher");
    }
}

impl Drop for Launch {
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

fn open_terminal() -> (OwnedFd, OwnedFd) {
    let master = rustix::fs::open(
        "/dev/ptmx",
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open a terminal");
    rustix::pty::grantpt(&master).expect("grant the terminal");
    rustix::pty::unlockpt(&master).expect("unlock the terminal");
    let name = rustix::pty::ptsname(&master, Vec::new()).expect("name the terminal");
    let slave = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open the terminal's slave");
    tcsetwinsize(
        &slave,
        Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .expect("size the terminal");
    (master, slave)
}

fn launcher_request(directory: &Path, bootstrap: &str) -> Vec<u8> {
    let path = |name: &str| directory.join(name).to_string_lossy().into_owned();
    let bootstrap_path = path("bootstrap");
    let config = json!({
        "argv": [
            b"/bin/bash".to_vec(),
            b"--noprofile".to_vec(),
            b"--norc".to_vec(),
            b"-i".to_vec(),
            b"-c".to_vec(),
            format!(". '{bootstrap_path}'\n").into_bytes(),
        ],
        "cwd": directory.as_os_str().as_bytes().to_vec(),
        "dimensions": {"rows": 24, "columns": 80},
        "control_path": path("control.sock"),
        "control_nonce": NONCE,
        "bootstrap_path": bootstrap_path,
        "bootstrap": bootstrap.as_bytes().to_vec(),
        "command_path": path("command"),
        "command": "exit 0",
    })
    .to_string();
    let mut request = u32::try_from(config.len())
        .expect("a bounded configuration")
        .to_le_bytes()
        .to_vec();
    request.extend_from_slice(config.as_bytes());
    request
}

fn shell_ready_turns_echo_back_on_after_startup_turned_it_off() {
    let directory = tempfile::tempdir().expect("create a directory");
    let directory = fs::canonicalize(directory.path()).expect("canonicalize the directory");
    let executable = env::current_exe().expect("the test executable");
    let marker = |event: &str| {
        format!(
            "'{}' '{CONTROL_MODE}' '{}' '{NONCE}' '{event}' || exit 125",
            executable.display(),
            directory.join("control.sock").display()
        )
    };
    let echo_state = "if stty -a | tr ' ' '\\n' | grep -qx -- -echo; then echo \"$1 echo-off\"; else echo \"$1 echo-on\"; fi";
    let bootstrap = format!(
        "set +x; state() {{ {echo_state}; }}; stty -echo; state before; {}; state after; {}; exit 0\n",
        marker("shell-ready"),
        marker("command-started")
    );
    let mut launch = Launch::start(&directory, &bootstrap);
    assert_eq!(launch.frame().0, SHELL_READY);
    let modes = tcgetattr(&launch.master).expect("read the terminal's modes");
    assert!(modes.local_modes.contains(LocalModes::ECHO));
    assert_eq!(launch.frame().0, COMMAND_STARTED);
    launch.send(2);
    assert_eq!(launch.frame(), (COMMAND_EXITED, 0));
    launch.liveness = None;
    let status = launch
        .exited
        .recv_timeout(LONG)
        .expect("the launcher exits in time")
        .expect("wait for the launcher");
    launch.launcher = None;
    assert!(status.success());
    let output = launch
        .output
        .recv_timeout(LONG)
        .expect("the terminal output");
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("before echo-off"), "{text:?}");
    assert!(text.contains("after echo-on"), "{text:?}");
}
