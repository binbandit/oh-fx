use std::io::{self, PipeReader, PipeWriter};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions};
use rustix::pty::OpenptFlags;
use rustix::termios::{self, Winsize};

const CURSOR_POSITION_REPORT: u16 = 6;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const SCROLLBACK_ROWS: usize = 1000;

type Terminal = Arc<Mutex<vt100::Parser<CursorReplies>>>;

pub struct PtyPair {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

impl PtyPair {
    pub fn open(rows: u16, cols: u16) -> io::Result<Self> {
        let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
        rustix::pty::grantpt(&master)?;
        rustix::pty::unlockpt(&master)?;
        let name = rustix::pty::ptsname(&master, Vec::new())?;
        let slave = rustix::fs::open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        termios::tcsetwinsize(&slave, winsize(rows, cols))?;
        Ok(Self { master, slave })
    }
}

pub struct PtySession {
    master: OwnedFd,
    child: Child,
    exit: OnceLock<ExitStatus>,
    terminal: Terminal,
    output: Arc<Mutex<Vec<u8>>>,
    reader_stop: PipeWriter,
    reader: Option<JoinHandle<()>>,
}

impl PtySession {
    pub fn spawn(mut command: Command, rows: u16, cols: u16) -> io::Result<Self> {
        let PtyPair { master, slave } = PtyPair::open(rows, cols)?;
        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        let (stop, reader_stop) = io::pipe()?;
        let reader_master = master.try_clone()?;
        let child = command.spawn()?;
        let terminal = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            SCROLLBACK_ROWS,
            CursorReplies::default(),
        )));
        let output = Arc::new(Mutex::new(Vec::new()));
        let reader = spawn_reader(
            reader_master,
            stop,
            Arc::clone(&terminal),
            Arc::clone(&output),
        );
        Ok(Self {
            master,
            child,
            exit: OnceLock::new(),
            terminal,
            output,
            reader_stop,
            reader: Some(reader),
        })
    }

    pub fn send(&self, bytes: &[u8]) {
        write_all(self.master.as_fd(), bytes);
    }

    pub fn screen(&self) -> String {
        lock(&self.terminal).screen().contents()
    }

    pub fn screen_rows(&self) -> Vec<String> {
        let terminal = lock(&self.terminal);
        let cols = terminal.screen().size().1;
        terminal
            .screen()
            .rows(0, cols)
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    pub fn output(&self) -> Vec<u8> {
        lock(&self.output).clone()
    }

    pub fn wait_for(
        &self,
        timeout: Duration,
        ready: impl Fn(&str) -> bool,
    ) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen();
            if ready(&screen) {
                return Ok(screen);
            }
            if Instant::now() >= deadline {
                return Err(screen);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        termios::tcsetwinsize(&self.master, winsize(rows, cols))?;
        lock(&self.terminal).screen_mut().set_size(rows, cols);
        self.signal(Signal::WINCH)
    }

    pub fn resume(&self) -> io::Result<()> {
        self.signal(Signal::CONT)
    }

    pub fn wait_until_stopped(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while let Some(pid) = self.unreaped_pid() {
            let options = WaitOptions::NOHANG | WaitOptions::UNTRACED;
            if let Ok(Some((_, status))) = rustix::process::waitpid(Some(pid), options) {
                if status.stopped() {
                    return true;
                }
                let _ = self.exit.set(ExitStatus::from_raw(status.as_raw()));
            } else if Instant::now() >= deadline {
                return false;
            } else {
                thread::sleep(POLL_INTERVAL);
            }
        }
        false
    }

    fn unreaped_pid(&self) -> Option<Pid> {
        if self.exit.get().is_some() {
            return None;
        }
        Pid::from_raw(i32::try_from(self.child.id()).unwrap_or(0))
    }

    fn signal(&self, signal: Signal) -> io::Result<()> {
        let pid = self.unreaped_pid().ok_or(Errno::SRCH)?;
        rustix::process::kill_process(pid, signal)?;
        Ok(())
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.exit.get() {
                return Some(*status);
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                let _ = self.exit.set(status);
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.exit.get().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = rustix::io::write(&self.reader_stop, &[0]);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[derive(Default)]
struct CursorReplies(Vec<u8>);

impl vt100::Callbacks for CursorReplies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        action: char,
    ) {
        if let (None, 'n', [[CURSOR_POSITION_REPORT]]) = (intermediate, action, params) {
            let (row, col) = screen.cursor_position();
            let reply = format!("\x1b[{};{}R", row + 1, col + 1);
            self.0.extend_from_slice(reply.as_bytes());
        }
    }
}

fn winsize(rows: u16, cols: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn write_all(fd: BorrowedFd<'_>, bytes: &[u8]) {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match rustix::io::write(fd, remaining) {
            Ok(written) => remaining = &remaining[written..],
            Err(Errno::INTR) => {}
            Err(_) => return,
        }
    }
}

fn spawn_reader(
    master: OwnedFd,
    stop: PipeReader,
    terminal: Terminal,
    output: Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while readable_before_stop(&master, &stop) {
            let count = match rustix::io::read(&master, &mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(count) => count,
            };
            let chunk = &buffer[..count];
            lock(&output).extend_from_slice(chunk);
            let replies = {
                let mut terminal = lock(&terminal);
                terminal.process(chunk);
                std::mem::take(&mut terminal.callbacks_mut().0)
            };
            write_all(master.as_fd(), &replies);
        }
    })
}

fn readable_before_stop(master: &OwnedFd, stop: &PipeReader) -> bool {
    let mut fds = [
        PollFd::new(master, PollFlags::IN),
        PollFd::new(stop, PollFlags::IN),
    ];
    loop {
        match poll(&mut fds, None) {
            Ok(_) => return fds[1].revents().is_empty(),
            Err(Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn shell(script: &str) -> PtySession {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        PtySession::spawn(command, 5, 40).unwrap()
    }

    #[test]
    fn sessions_render_child_output_and_answer_cursor_queries() {
        let mut session = shell("printf 'hello\\033[6n'; sleep 0.2; printf 'done\\n'");
        let screen = session
            .wait_for(WAIT, |screen| screen.contains("done"))
            .unwrap();
        assert!(screen.starts_with("hello"));
        assert!(session.wait_exit(WAIT).is_some());
        assert!(
            session
                .output()
                .windows(b"\x1b[6n".len())
                .any(|window| window == b"\x1b[6n")
        );
        assert_eq!(session.screen_rows().len(), 5);
    }

    #[test]
    fn cursor_queries_split_across_reads_are_answered_with_the_position_at_the_query() {
        let session = shell(concat!(
            "stty raw -echo min 0 time 20; ",
            "printf 'hello\\033[6nworld'; ",
            "first=$(dd bs=1 count=6 2>/dev/null | tr '\\033' E); ",
            "printf '\\r\\nab\\033['; sleep 0.15; printf '6nmore'; ",
            "second=$(dd bs=1 count=6 2>/dev/null | tr '\\033' E); ",
            "printf '\\r\\nreplies %s %s end\\r\\n' \"$first\" \"$second\"",
        ));
        let screen = session
            .wait_for(WAIT, |screen| screen.contains(" end"))
            .unwrap();
        assert!(screen.contains("replies E[1;6R E[2;3R end"), "{screen}");
    }

    #[test]
    fn stopped_children_are_observed_and_resumed() {
        let mut session = shell("kill -STOP $$; printf 'resumed\\n'");
        assert!(session.wait_until_stopped(WAIT));
        session.resume().unwrap();
        session
            .wait_for(WAIT, |screen| screen.contains("resumed"))
            .unwrap();
        assert!(session.wait_exit(WAIT).is_some());
    }

    #[test]
    fn children_that_exit_instead_of_stopping_keep_their_exit_status() {
        let mut session = shell("printf 'exited\\n'; exit 7");
        assert!(!session.wait_until_stopped(WAIT));
        let status = session.wait_exit(WAIT).unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn dropping_a_session_does_not_wait_for_descendants_that_hold_the_terminal() {
        let session = shell("sleep 30 & printf 'descendant %s ready\\n' \"$!\"; wait");
        let screen = session
            .wait_for(WAIT, |screen| screen.contains("ready"))
            .unwrap();
        let descendant = screen
            .split_whitespace()
            .nth(1)
            .and_then(|pid| pid.parse().ok())
            .and_then(Pid::from_raw)
            .unwrap();
        let (done, finished) = mpsc::channel();
        let dropping = thread::spawn(move || {
            drop(session);
            let _ = done.send(());
        });
        let outcome = finished.recv_timeout(Duration::from_secs(1));
        let _ = rustix::process::kill_process(descendant, Signal::KILL);
        dropping.join().unwrap();
        assert!(
            outcome.is_ok(),
            "dropping the session waited for a descendant that holds the terminal"
        );
    }
}
