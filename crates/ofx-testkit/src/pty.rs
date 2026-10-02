use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal, WaitOptions};
use rustix::pty::OpenptFlags;
use rustix::termios::{self, Winsize};

const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const SCROLLBACK_ROWS: usize = 1000;

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
    terminal: Arc<Mutex<vt100::Parser>>,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
}

impl PtySession {
    pub fn spawn(mut command: Command, rows: u16, cols: u16) -> io::Result<Self> {
        let PtyPair { master, slave } = PtyPair::open(rows, cols)?;
        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        let child = command.spawn()?;
        let terminal = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_ROWS)));
        let output = Arc::new(Mutex::new(Vec::new()));
        let reader = spawn_reader(
            master.try_clone()?,
            Arc::clone(&terminal),
            Arc::clone(&output),
        );
        Ok(Self {
            master,
            child,
            terminal,
            output,
            reader: Some(reader),
        })
    }

    pub fn send(&self, bytes: &[u8]) {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match rustix::io::write(&self.master, remaining) {
                Ok(written) => remaining = &remaining[written..],
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => return,
            }
        }
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
        let Some(pid) = self.pid() else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        loop {
            let options = WaitOptions::NOHANG | WaitOptions::UNTRACED;
            if let Ok(Some((_, status))) = rustix::process::waitpid(Some(pid), options)
                && status.stopped()
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn pid(&self) -> Option<Pid> {
        Pid::from_raw(i32::try_from(self.child.id()).unwrap_or(0))
    }

    fn signal(&self, signal: Signal) -> io::Result<()> {
        if let Some(pid) = self.pid() {
            rustix::process::kill_process(pid, signal)?;
        }
        Ok(())
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
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
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
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

fn spawn_reader(
    master: OwnedFd,
    terminal: Arc<Mutex<vt100::Parser>>,
    output: Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            let count = match rustix::io::read(&master, &mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(count) => count,
            };
            let chunk = &buffer[..count];
            lock(&output).extend_from_slice(chunk);
            let mut terminal = lock(&terminal);
            let mut rest = chunk;
            while let Some(position) = find(rest, CURSOR_POSITION_QUERY) {
                let end = position + CURSOR_POSITION_QUERY.len();
                terminal.process(&rest[..end]);
                let (row, col) = terminal.screen().cursor_position();
                let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                let _ = rustix::io::write(master.as_fd(), reply.as_bytes());
                rest = &rest[end..];
            }
            terminal.process(rest);
        }
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_render_child_output_and_answer_cursor_queries() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'hello\\033[6n'; sleep 0.2; printf 'done\\n'"]);
        let mut session = PtySession::spawn(command, 5, 20).unwrap();
        let screen = session
            .wait_for(Duration::from_secs(5), |screen| screen.contains("done"))
            .unwrap();
        assert!(screen.starts_with("hello"));
        assert!(session.wait_exit(Duration::from_secs(5)).is_some());
        assert!(
            session
                .output()
                .windows(CURSOR_POSITION_QUERY.len())
                .any(|window| window == CURSOR_POSITION_QUERY)
        );
        assert_eq!(session.screen_rows().len(), 5);
    }

    #[test]
    fn stopped_children_are_observed_and_resumed() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "kill -STOP $$; printf 'resumed\\n'"]);
        let mut session = PtySession::spawn(command, 5, 20).unwrap();
        assert!(session.wait_until_stopped(Duration::from_secs(5)));
        session.resume().unwrap();
        session
            .wait_for(Duration::from_secs(5), |screen| screen.contains("resumed"))
            .unwrap();
        assert!(session.wait_exit(Duration::from_secs(5)).is_some());
    }
}
