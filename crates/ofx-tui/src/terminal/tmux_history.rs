use std::env;
use std::ffi::OsString;
use std::io::Read;
use std::os::fd::BorrowedFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::process::{Pid, Signal, kill_process_group};

use super::Terminal;

const CLEAR_SCREEN_AND_HISTORY: &[u8] = b"\x1b[0m\x1b[2J\x1b[3J\x1b[H";
const SCREEN_CHECKS: usize = 5;
const SCREEN_CHECK_INTERVAL: Duration = Duration::from_millis(5);
const CALL_LIMIT: Duration = Duration::from_millis(250);
const EXIT_POLL: Duration = Duration::from_millis(5);
const READ_CHUNK_BYTES: usize = 4096;
const CLEAR_HISTORY_UNLESS_IN_MODE: &str = "#{?pane_in_mode,,clear-history -t #{pane_id}}";

pub(crate) struct TmuxHistory {
    program: OsString,
    pane: OsString,
}

struct Interrupted;

struct Finished {
    status: ExitStatus,
    stdout: Vec<u8>,
}

impl TmuxHistory {
    pub(crate) fn detect() -> Option<Self> {
        env::var_os("TMUX")?;
        Some(Self {
            program: OsString::from("tmux"),
            pane: env::var_os("TMUX_PANE")?,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_program(program: impl Into<OsString>, pane: &str) -> Self {
        Self {
            program: program.into(),
            pane: OsString::from(pane),
        }
    }

    pub(crate) fn clear(&self, terminal: &Terminal) {
        if terminal.write_all(CLEAR_SCREEN_AND_HISTORY).is_err() {
            return;
        }
        let interrupt = terminal.fatal_signal_wakeup();
        if self.wait_for_blank_pane(interrupt).is_ok() {
            let _ = self.clear_history(interrupt);
        }
    }

    fn wait_for_blank_pane(&self, interrupt: Option<BorrowedFd<'_>>) -> Result<(), Interrupted> {
        for check in 1..=SCREEN_CHECKS {
            if self.pane_is_blank(interrupt)?.unwrap_or(true) {
                return Ok(());
            }
            if check < SCREEN_CHECKS {
                thread::sleep(SCREEN_CHECK_INTERVAL);
            }
        }
        Ok(())
    }

    fn pane_is_blank(
        &self,
        interrupt: Option<BorrowedFd<'_>>,
    ) -> Result<Option<bool>, Interrupted> {
        let finished = run_bounded(self.command(["capture-pane", "-p", "-t"]), interrupt)?;
        Ok(finished
            .filter(|finished| finished.status.success())
            .map(|finished| {
                finished
                    .stdout
                    .iter()
                    .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
            }))
    }

    fn clear_history(&self, interrupt: Option<BorrowedFd<'_>>) -> Result<(), Interrupted> {
        let mut command = self.command(["run-shell", "-C", "-t"]);
        command.arg(CLEAR_HISTORY_UNLESS_IN_MODE);
        run_bounded(command, interrupt).map(drop)
    }

    fn command<const N: usize>(&self, arguments: [&str; N]) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(arguments)
            .arg(&self.pane)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        command
    }
}

fn run_bounded(
    mut command: Command,
    interrupt: Option<BorrowedFd<'_>>,
) -> Result<Option<Finished>, Interrupted> {
    if interrupt.is_some_and(|interrupt| readable(interrupt, Duration::ZERO)) {
        return Err(Interrupted);
    }
    let Ok(mut child) = command.spawn() else {
        return Ok(None);
    };
    let deadline = Instant::now() + CALL_LIMIT;
    let mut output = child.stdout.take();
    let mut stdout = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            stop(child);
            return Ok(None);
        }
        let wait = if output.is_some() {
            remaining
        } else {
            remaining.min(EXIT_POLL)
        };
        let (output_ready, interrupted) = poll_call(output.as_ref(), interrupt, wait);
        if interrupted {
            stop(child);
            return Err(Interrupted);
        }
        if output_ready && let Some(pipe) = &mut output {
            let mut chunk = [0_u8; READ_CHUNK_BYTES];
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => output = None,
                Ok(count) => stdout.extend_from_slice(&chunk[..count]),
            }
        }
        if output.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(Some(Finished { status, stdout })),
                Ok(None) => {}
                Err(_) => {
                    stop(child);
                    return Ok(None);
                }
            }
        }
    }
}

fn poll_call(
    output: Option<&ChildStdout>,
    interrupt: Option<BorrowedFd<'_>>,
    wait: Duration,
) -> (bool, bool) {
    let timeout = Timespec::try_from(wait).ok();
    let mut fds = Vec::with_capacity(2);
    if let Some(output) = output {
        fds.push(PollFd::new(output, PollFlags::IN));
    }
    if let Some(interrupt) = &interrupt {
        fds.push(PollFd::new(interrupt, PollFlags::IN));
    }
    if rustix::event::poll(&mut fds, timeout.as_ref()).is_err() {
        return (false, false);
    }
    let ready = |fd: &PollFd<'_>| !fd.revents().is_empty();
    let output_ready = output.is_some() && fds.first().is_some_and(ready);
    let interrupted = interrupt.is_some() && fds.last().is_some_and(ready);
    (output_ready, interrupted)
}

fn readable(fd: BorrowedFd<'_>, wait: Duration) -> bool {
    let timeout = Timespec::try_from(wait).ok();
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    rustix::event::poll(&mut fds, timeout.as_ref()).is_ok_and(|ready| ready > 0)
}

fn stop(mut child: Child) {
    if let Some(group) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) {
        let _ = kill_process_group(group, Signal::KILL);
    }
    let _ = child.kill();
    thread::spawn(move || child.wait());
}

#[cfg(test)]
pub(crate) mod fake_tmux {
    use std::fs;
    use std::io::{ErrorKind, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    use rustix::fs::{Mode, OFlags};

    pub(crate) struct FakeTmux {
        directory: tempfile::TempDir,
        alive: Option<fs::File>,
    }

    impl FakeTmux {
        pub(crate) fn new(captures_before_blank: usize, capture_status: u8) -> Self {
            Self::with_body(&format!(
                "if [ \"$1\" = capture-pane ]; then\n\
                 count=$(($(cat \"$ROOT/count\" 2>/dev/null || echo 0) + 1))\n\
                 echo \"$count\" > \"$ROOT/count\"\n\
                 [ \"$count\" -le {captures_before_blank} ] && echo 'still drawing'\n\
                 exit {capture_status}\n\
                 fi\n"
            ))
        }

        pub(crate) fn hanging() -> Self {
            let mut fake = Self::with_body("/bin/sleep 30 &\nwait\n");
            let alive = fake.directory.path().join("alive");
            assert!(
                Command::new("mkfifo")
                    .arg(&alive)
                    .status()
                    .unwrap()
                    .success()
            );
            let reader = rustix::fs::open(
                &alive,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            fake.alive = Some(fs::File::from(reader));
            fake
        }

        fn with_body(body: &str) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().display();
            let script = format!(
                "#!/bin/sh\n\
                 ROOT='{root}'\n\
                 if [ -p \"$ROOT/alive\" ]; then exec 3>\"$ROOT/alive\"; fi\n\
                 printf '%s|' \"$@\" >> \"$ROOT/calls\"\n\
                 printf '\\n' >> \"$ROOT/calls\"\n\
                 {body}"
            );
            let program = directory.path().join("tmux");
            fs::write(&program, script).unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                directory,
                alive: None,
            }
        }

        pub(crate) fn every_call_exits_within(&mut self, limit: Duration) -> bool {
            let pipe = self.alive.as_mut().expect("a hanging fake tmux");
            let deadline = Instant::now() + limit;
            let mut byte = [0_u8; 1];
            loop {
                match pipe.read(&mut byte) {
                    Ok(0) => return true,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        if Instant::now() > deadline {
                            return false;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    other => panic!("unexpected read from the liveness pipe: {other:?}"),
                }
            }
        }

        pub(crate) fn program(&self) -> PathBuf {
            self.directory.path().join("tmux")
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            fs::read_to_string(self.directory.path().join("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake_tmux::FakeTmux;
    use super::*;
    use crate::terminal::test_pty;

    const CAPTURE: &str = "capture-pane|-p|-t|%7|";
    const CLEAR: &str = "run-shell|-C|-t|%7|#{?pane_in_mode,,clear-history -t #{pane_id}}|";

    fn cleared(fake: &FakeTmux) -> Vec<u8> {
        let pty = test_pty::open();
        let terminal = test_pty::terminal(&pty);
        TmuxHistory::with_program(fake.program(), "%7").clear(&terminal);
        test_pty::read_written(&pty)
    }

    #[test]
    fn the_screen_is_cleared_then_history_once_the_pane_shows_blank() {
        let fake = FakeTmux::new(2, 0);
        assert_eq!(cleared(&fake), CLEAR_SCREEN_AND_HISTORY);
        assert_eq!(fake.calls(), [CAPTURE, CAPTURE, CAPTURE, CLEAR]);
    }

    #[test]
    fn history_is_cleared_after_five_checks_of_a_pane_that_stays_drawn() {
        let fake = FakeTmux::new(100, 0);
        cleared(&fake);
        let mut expected = vec![CAPTURE; 5];
        expected.push(CLEAR);
        assert_eq!(fake.calls(), expected);
    }

    #[test]
    fn a_failed_capture_stops_waiting_but_still_clears_history() {
        let fake = FakeTmux::new(100, 1);
        cleared(&fake);
        assert_eq!(fake.calls(), [CAPTURE, CLEAR]);
    }

    #[test]
    fn a_tmux_call_that_never_finishes_is_killed_with_its_children_once_its_wait_runs_out() {
        let mut fake = FakeTmux::hanging();
        let begun = Instant::now();
        assert_eq!(cleared(&fake), CLEAR_SCREEN_AND_HISTORY);
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "{:?}",
            begun.elapsed()
        );
        assert_eq!(fake.calls(), [CAPTURE, CLEAR]);
        assert!(fake.every_call_exits_within(Duration::from_secs(5)));
    }

    #[test]
    fn a_fatal_signal_ends_the_wait_for_tmux_and_skips_the_rest() {
        let mut fake = FakeTmux::hanging();
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        let (wakeup, mut signal) = std::io::pipe().unwrap();
        terminal.abort_writes_when_readable(wakeup.into());
        let signaller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            std::io::Write::write_all(&mut signal, b"x").unwrap();
            signal
        });
        TmuxHistory::with_program(fake.program(), "%7").clear(&terminal);
        drop(signaller.join().unwrap());
        assert_eq!(fake.calls(), [CAPTURE]);
        assert!(fake.every_call_exits_within(Duration::from_secs(5)));
    }

    #[test]
    fn a_missing_tmux_leaves_only_the_screen_clear() {
        let missing = TmuxHistory::with_program("/nonexistent/tmux", "%7");
        let pty = test_pty::open();
        missing.clear(&test_pty::terminal(&pty));
        assert_eq!(test_pty::read_written(&pty), CLEAR_SCREEN_AND_HISTORY);
    }
}
