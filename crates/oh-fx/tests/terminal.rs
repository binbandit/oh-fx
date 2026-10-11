use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use ofx_exec::{
    ControlFrame, ControlKind, Dimensions, LaunchedTerminal, Liveness, TerminalLaunch,
    launch_terminal, read_control_frame,
};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};

const OH_FX: &str = env!("CARGO_BIN_EXE_oh-fx");
const LONG: Duration = Duration::from_secs(20);

fn hidden_mode(args: &[&str]) -> Output {
    Command::new(OH_FX)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run oh-fx")
}

#[test]
fn the_hidden_terminal_launcher_starts_nothing_without_a_config() {
    let output = hidden_mode(&["--oh-fx-internal-terminal-launcher"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn the_hidden_terminal_control_marker_needs_its_launcher() {
    let nonce = "a".repeat(32);
    let output = hidden_mode(&[
        "--oh-fx-internal-terminal-control",
        "/nonexistent/control.sock",
        &nonce,
        "shell-ready",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

struct Terminal {
    liveness: Option<Liveness>,
    launcher: Option<Pid>,
    shell: Option<Pid>,
    ended: bool,
    frames: Receiver<ControlFrame>,
    exited: Receiver<io::Result<ExitStatus>>,
    output: Receiver<Vec<u8>>,
}

impl Terminal {
    fn launch(cwd: &Path, command: &str) -> Self {
        let LaunchedTerminal {
            master,
            mut control,
            mut liveness,
            mut launcher,
        } = launch_terminal(&TerminalLaunch {
            executable: Path::new(OH_FX),
            shell: Path::new("/bin/bash"),
            clean_start: true,
            cwd,
            command,
            dimensions: Dimensions {
                rows: 24,
                columns: 80,
            },
        })
        .expect("launch a terminal");
        rustix::io::ioctl_fionbio(&master, false).expect("read the terminal blocking");
        let (output_sender, output) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = File::from(master).read_to_end(&mut bytes);
            let _ = output_sender.send(bytes);
        });
        let (frame_sender, frames) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(frame) = read_control_frame(&mut control) {
                if frame_sender.send(frame).is_err() {
                    break;
                }
            }
        });
        let pid = i32::try_from(launcher.id()).ok().and_then(Pid::from_raw);
        let (status_sender, exited) = mpsc::channel();
        thread::spawn(move || {
            let _ = status_sender.send(launcher.wait());
        });
        liveness.release_launcher().expect("release the launcher");
        Self {
            liveness: Some(liveness),
            launcher: pid,
            shell: None,
            ended: false,
            frames,
            exited,
            output,
        }
    }

    fn frame(&mut self) -> ControlFrame {
        let frame = self
            .frames
            .recv_timeout(LONG)
            .expect("a control frame in time");
        match frame.kind {
            ControlKind::ShellReady | ControlKind::CommandStarted => {
                self.shell = i32::try_from(frame.value).ok().and_then(Pid::from_raw);
            }
            ControlKind::Prepared => {}
            ControlKind::CommandExited
            | ControlKind::CommandSignal
            | ControlKind::StartupFailed
            | ControlKind::InvalidTerm => self.ended = true,
        }
        frame
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

#[test]
fn the_hidden_terminal_launcher_runs_a_command_in_a_terminal() {
    let directory = tempfile::tempdir().expect("create a directory");
    let mut terminal = Terminal::launch(directory.path(), "printf 'launched by %s' oh-fx; exit 3");
    let ready = terminal.frame();
    let started = terminal.frame();
    assert_eq!(
        [ready.kind, started.kind],
        [ControlKind::ShellReady, ControlKind::CommandStarted]
    );
    assert_eq!(started.value, ready.value);
    terminal
        .liveness
        .as_mut()
        .expect("the terminal is still owned")
        .release_command()
        .expect("release the command");
    assert_eq!(
        terminal.frame(),
        ControlFrame {
            kind: ControlKind::CommandExited,
            value: 3
        }
    );
    terminal.liveness = None;
    assert!(terminal.launcher_exit().success());
    let output = terminal
        .output
        .recv_timeout(LONG)
        .expect("the terminal output");
    assert!(
        String::from_utf8_lossy(&output).contains("launched by oh-fx"),
        "{output:?}"
    );
}
