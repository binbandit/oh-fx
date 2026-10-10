use std::io::{self, PipeReader, PipeWriter};
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions};
use rustix::pty::OpenptFlags;
use rustix::termios::{self, LocalModes, Winsize};

use crate::host_stand_ins::HostStandIns;

const CURSOR_POSITION_REPORT: u16 = 6;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const READ_CHUNK_BYTES: usize = 4096;
const SCROLLBACK_ROWS: usize = 1000;
const FULL_OUTPUT_ROUNDS: usize = 5;
const OPEN_ATTEMPTS: usize = 50;
const OPEN_RETRY: Duration = Duration::from_millis(10);

type Terminal = Arc<Mutex<Display>>;

const SYNC_MARKER_PREFIX: &[u8] = b"\x1b[?2026";

struct Display {
    parser: vt100::Parser<CursorReplies>,
    shown: vt100::Screen,
    marker_matched: usize,
    synchronized: bool,
}

impl Display {
    fn new(rows: u16, cols: u16) -> Self {
        let parser = vt100::Parser::new_with_callbacks(
            rows,
            cols,
            SCROLLBACK_ROWS,
            CursorReplies::default(),
        );
        let shown = parser.screen().clone();
        Self {
            parser,
            shown,
            marker_matched: 0,
            synchronized: false,
        }
    }

    fn process(&mut self, chunk: &[u8]) {
        let mut start = 0;
        for (index, byte) in chunk.iter().enumerate() {
            let Some(opens) = self.marker(*byte) else {
                continue;
            };
            let marker_start = (index + 1)
                .saturating_sub(SYNC_MARKER_PREFIX.len() + 1)
                .max(start);
            self.parser.process(&chunk[start..marker_start]);
            self.show_unless_synchronized();
            self.synchronized = opens;
            self.parser.process(&chunk[marker_start..=index]);
            self.show_unless_synchronized();
            start = index + 1;
        }
        self.parser.process(&chunk[start..]);
        self.show_unless_synchronized();
    }

    fn marker(&mut self, byte: u8) -> Option<bool> {
        if self.marker_matched == SYNC_MARKER_PREFIX.len() {
            self.marker_matched = 0;
            match byte {
                b'h' => return Some(true),
                b'l' => return Some(false),
                _ => {}
            }
        }
        if byte == SYNC_MARKER_PREFIX[self.marker_matched] {
            self.marker_matched += 1;
        } else {
            self.marker_matched = usize::from(byte == SYNC_MARKER_PREFIX[0]);
        }
        None
    }

    fn show_unless_synchronized(&mut self) {
        if !self.synchronized {
            self.shown = self.parser.screen().clone();
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.show_unless_synchronized();
    }
}

pub struct PtyPair {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

impl PtyPair {
    pub fn open(rows: u16, cols: u16) -> io::Result<Self> {
        let master = open_master()?;
        rustix::pty::grantpt(&master)?;
        rustix::pty::unlockpt(&master)?;
        let slave = open_slave(&master)?;
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
    stall: Arc<Stall>,
    held_slave: Option<OwnedFd>,
    reader_stop: PipeWriter,
    reader: Option<JoinHandle<()>>,
    host: HostStandIns,
}

#[derive(Default)]
struct Stall {
    state: Mutex<StallState>,
    changed: Condvar,
}

#[derive(Default)]
struct StallState {
    needle: Vec<u8>,
    tripped: bool,
    parked: bool,
}

impl Stall {
    fn arm(&self, needle: &[u8]) {
        let mut state = lock(&self.state);
        state.needle = needle.to_vec();
        state.tripped = false;
    }

    fn trip_if_shown(&self, output: &[u8], new_from: usize) {
        let mut state = lock(&self.state);
        if state.needle.is_empty() {
            return;
        }
        let start = new_from.saturating_sub(state.needle.len() - 1);
        if output[start..]
            .windows(state.needle.len())
            .any(|window| window == state.needle)
        {
            state.needle.clear();
            state.tripped = true;
        }
    }

    fn park_while_tripped(&self) {
        let mut state = lock(&self.state);
        while state.tripped {
            state.parked = true;
            self.changed.notify_all();
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.parked = false;
    }

    fn wait_until_parked(&self, deadline: Instant) -> bool {
        let mut state = lock(&self.state);
        while !state.parked {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    fn release(&self) {
        let mut state = lock(&self.state);
        state.needle.clear();
        state.tripped = false;
        self.changed.notify_all();
    }
}

impl PtySession {
    pub fn spawn(mut command: Command, rows: u16, cols: u16) -> io::Result<Self> {
        let host = HostStandIns::install(&mut command)?;
        let PtyPair { master, slave } = PtyPair::open(rows, cols)?;
        rustix::fs::fcntl_setfl(
            &master,
            rustix::fs::fcntl_getfl(&master)? | OFlags::NONBLOCK,
        )?;
        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        let (stop, reader_stop) = io::pipe()?;
        let terminal = Arc::new(Mutex::new(Display::new(rows, cols)));
        let output = Arc::new(Mutex::new(Vec::new()));
        let stall = Arc::new(Stall::default());
        let reader = Reader {
            master: master.try_clone()?,
            stop,
            terminal: Arc::clone(&terminal),
            output: Arc::clone(&output),
            stall: Arc::clone(&stall),
            replies: Vec::new(),
        };
        let child = command.spawn()?;
        Ok(Self {
            master,
            child,
            exit: OnceLock::new(),
            terminal,
            output,
            stall,
            held_slave: None,
            reader_stop,
            reader: Some(thread::spawn(move || reader.run())),
            host,
        })
    }

    pub fn stall_output_after(&mut self, needle: &[u8]) -> io::Result<()> {
        self.held_slave = Some(open_slave(&self.master)?);
        self.stall.arm(needle);
        Ok(())
    }

    pub fn wait_for_pending_output(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        if !self.stall.wait_until_parked(deadline) {
            return false;
        }
        let mut fds = [PollFd::new(&self.master, PollFlags::IN)];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = Timespec::try_from(remaining).unwrap_or_default();
            match poll(&mut fds, Some(&timeout)) {
                Err(Errno::INTR) => {}
                result => return result.is_ok() && fds[0].revents().contains(PollFlags::IN),
            }
        }
    }

    pub fn fill_stalled_output(&self) -> io::Result<usize> {
        if self.held_slave.is_none() {
            return Err(io::ErrorKind::NotConnected.into());
        }
        let name = rustix::pty::ptsname(&self.master, Vec::new())?;
        let slave = rustix::fs::open(
            name.as_c_str(),
            OFlags::WRONLY | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let filler = [b'#'; READ_CHUNK_BYTES];
        let mut written = 0;
        let mut full_rounds = 0;
        while full_rounds < FULL_OUTPUT_ROUNDS {
            match rustix::io::write(&slave, &filler) {
                Ok(count) => {
                    written += count;
                    full_rounds = 0;
                }
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => {
                    full_rounds += 1;
                    thread::sleep(POLL_INTERVAL);
                }
                Err(errno) => return Err(errno.into()),
            }
        }
        Ok(written)
    }

    pub fn drain_output(&mut self, timeout: Duration) -> bool {
        self.stall.release();
        self.held_slave = None;
        let deadline = Instant::now() + timeout;
        while !self.reader.as_ref().is_none_or(JoinHandle::is_finished) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(POLL_INTERVAL);
        }
        true
    }

    pub fn cooked(&self) -> io::Result<bool> {
        let modes = match &self.held_slave {
            Some(slave) => termios::tcgetattr(slave)?,
            None => termios::tcgetattr(open_slave(&self.master)?)?,
        }
        .local_modes;
        Ok(modes.contains(LocalModes::ICANON | LocalModes::ECHO))
    }

    pub fn terminate(&self) -> io::Result<()> {
        self.signal(Signal::TERM)
    }

    pub fn kill(&self) -> io::Result<()> {
        self.signal(Signal::KILL)
    }

    pub fn send(&self, bytes: &[u8]) {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match rustix::io::write(&self.master, remaining) {
                Ok(written) => remaining = &remaining[written..],
                Err(Errno::AGAIN) if self.wait_until_writable() => {}
                Err(Errno::INTR) => {}
                Err(_) => return,
            }
        }
    }

    fn wait_until_writable(&self) -> bool {
        let mut fds = [PollFd::new(&self.master, PollFlags::OUT)];
        loop {
            match poll(&mut fds, None) {
                Ok(_) => {
                    let closed = PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL;
                    return !fds[0].revents().intersects(closed);
                }
                Err(Errno::INTR) => {}
                Err(_) => return false,
            }
        }
    }

    pub fn screen(&self) -> String {
        lock(&self.terminal).shown.contents()
    }

    pub fn screen_rows(&self) -> Vec<String> {
        let terminal = lock(&self.terminal);
        let cols = terminal.shown.size().1;
        terminal
            .shown
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
        lock(&self.terminal).resize(rows, cols);
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
        self.stall.release();
        self.held_slave = None;
        let _ = rustix::io::write(&self.reader_stop, &[0]);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let calls = self.host.calls();
        assert!(
            calls.is_empty() || thread::panicking(),
            "the PTY child ran host commands that only a stand-in answered:\n{calls}"
        );
    }
}

struct Reader {
    master: OwnedFd,
    stop: PipeReader,
    terminal: Terminal,
    output: Arc<Mutex<Vec<u8>>>,
    stall: Arc<Stall>,
    replies: Vec<u8>,
}

impl Reader {
    fn run(mut self) {
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        while self.wait_for_master() && self.read(&mut buffer) {
            self.flush_replies();
            self.stall.park_while_tripped();
        }
    }

    fn wait_for_master(&self) -> bool {
        let events = if self.replies.is_empty() {
            PollFlags::IN
        } else {
            PollFlags::IN | PollFlags::OUT
        };
        let mut fds = [
            PollFd::new(&self.master, events),
            PollFd::new(&self.stop, PollFlags::IN),
        ];
        loop {
            match poll(&mut fds, None) {
                Ok(_) => return fds[1].revents().is_empty(),
                Err(Errno::INTR) => {}
                Err(_) => return false,
            }
        }
    }

    fn read(&mut self, buffer: &mut [u8]) -> bool {
        match rustix::io::read(&self.master, &mut *buffer) {
            Ok(0) => false,
            Ok(count) => {
                self.record(&buffer[..count]);
                true
            }
            Err(Errno::AGAIN | Errno::INTR) => true,
            Err(_) => false,
        }
    }

    fn record(&mut self, chunk: &[u8]) {
        {
            let mut output = lock(&self.output);
            let new_from = output.len();
            output.extend_from_slice(chunk);
            self.stall.trip_if_shown(&output, new_from);
        }
        let mut terminal = lock(&self.terminal);
        terminal.process(chunk);
        self.replies.append(&mut terminal.parser.callbacks_mut().0);
    }

    fn flush_replies(&mut self) {
        while !self.replies.is_empty() {
            match rustix::io::write(&self.master, &self.replies) {
                Ok(0) | Err(Errno::AGAIN) => return,
                Ok(written) => {
                    self.replies.drain(..written);
                }
                Err(Errno::INTR) => {}
                Err(_) => self.replies.clear(),
            }
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

fn open_master() -> io::Result<OwnedFd> {
    let mut attempt = 1;
    loop {
        match rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY) {
            Err(error) if attempt < OPEN_ATTEMPTS && released_by_another_terminal(error) => {
                attempt += 1;
                thread::sleep(OPEN_RETRY);
            }
            opened => return Ok(opened?),
        }
    }
}

fn released_by_another_terminal(error: Errno) -> bool {
    error.raw_os_error().abs() == Errno::NXIO.raw_os_error()
}

fn open_slave(master: &OwnedFd) -> io::Result<OwnedFd> {
    let name = rustix::pty::ptsname(master, Vec::new())?;
    Ok(rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn shell(script: &str) -> PtySession {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]).env("PATH", "/usr/bin:/bin");
        PtySession::spawn(command, 5, 40).unwrap()
    }

    fn flood_of_cursor_queries(count: usize) -> String {
        let queries = "\\033[6n".repeat(10);
        format!(
            "i=0; while [ \"$i\" -lt {} ]; do printf '{queries}'; i=$((i + 1)); done",
            count / 10
        )
    }

    fn ready_descendant(session: &PtySession) -> Pid {
        session
            .wait_for(WAIT, |screen| screen.contains("ready"))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .and_then(|pid| pid.parse().ok())
            .and_then(Pid::from_raw)
            .unwrap()
    }

    fn drops_while_descendant_lives(session: PtySession, descendant: Pid) -> bool {
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            drop(session);
            let _ = done.send(());
        });
        let dropped = finished.recv_timeout(Duration::from_secs(1)).is_ok();
        let _ = rustix::process::kill_process(descendant, Signal::KILL);
        dropped
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_child_that_runs_a_host_command_reaches_only_its_stand_in_and_fails_at_drop() {
        let mut session = shell("pbcopy </dev/null; open https://example.invalid; echo \"ran $?\"");
        session
            .wait_for(WAIT, |screen| screen.contains("ran 1"))
            .unwrap();
        assert!(session.wait_exit(WAIT).is_some());
        let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(session)))
        else {
            panic!("a session whose child reached host commands was dropped quietly");
        };
        let message = payload.downcast_ref::<String>().unwrap();
        assert!(
            message.ends_with("answered:\npbcopy \nopen https://example.invalid\n"),
            "{message}"
        );
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
    fn a_synchronized_frame_shows_only_once_it_is_whole() {
        let session = shell(concat!(
            "printf 'before\\n'; sleep 0.3; ",
            "printf '\\033[?20'; sleep 0.3; ",
            "printf '26hhalf'; sleep 0.3; ",
            "printf ' whole\\033[?2026l'; sleep 0.3; ",
            "printf 'after'; exec sleep 10",
        ));
        session
            .wait_for(WAIT, |screen| screen.contains("before"))
            .unwrap();
        let seen = Mutex::new(Vec::new());
        let finished = session.wait_for(WAIT, |screen| {
            lock(&seen).push(screen.to_owned());
            screen.contains("after")
        });
        let seen = seen.into_inner().unwrap();
        assert!(finished.is_ok(), "{seen:?}");
        assert!(
            seen.iter()
                .all(|screen| !screen.contains("half") || screen.contains("half whole")),
            "{seen:?}"
        );
        assert!(seen.iter().any(|screen| screen.contains("half whole")));
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
    fn stalled_output_stays_pending_until_the_child_is_terminated() {
        let mut session =
            shell("read line; printf 'ready\\n'; read line; printf 'pending'; exec sleep 10");
        session.stall_output_after(b"ready\r\n").unwrap();
        session.send(b"start\n");
        let deadline = Instant::now() + WAIT;
        while !session.output().ends_with(b"ready\r\n") {
            assert!(Instant::now() < deadline, "the child never got ready");
            thread::sleep(POLL_INTERVAL);
        }
        let seen = session.output().len();
        session.send(b"go\n");
        assert!(
            session.wait_for_pending_output(WAIT),
            "the child output never queued"
        );
        assert_eq!(session.output().len(), seen);
        assert!(session.cooked().unwrap());
        session.terminate().unwrap();
        let status = session.wait_exit(WAIT).unwrap();
        assert_eq!(status.signal(), Some(15));
        assert_eq!(session.output().len(), seen);
        assert!(session.drain_output(WAIT));
        assert!(session.output()[seen..].starts_with(b"go\r\n"));
    }

    #[test]
    fn a_full_output_queue_holds_the_child_writes_until_drained() {
        let mut session =
            shell("read line; printf 'ready\\n'; read line; printf 'after\\n'; exec sleep 10");
        assert_eq!(
            session.fill_stalled_output().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        session.stall_output_after(b"ready\r\n").unwrap();
        session.send(b"start\n");
        let deadline = Instant::now() + WAIT;
        while !session.output().ends_with(b"ready\r\n") {
            assert!(Instant::now() < deadline, "the child never got ready");
            thread::sleep(POLL_INTERVAL);
        }
        assert!(session.fill_stalled_output().unwrap() > 0);
        session.send(b"go\n");
        thread::sleep(Duration::from_millis(200));
        session.terminate().unwrap();
        assert!(session.drain_output(WAIT));
        let output = session.output();
        assert!(output.windows(5).any(|window| window == b"#####"));
    }

    #[test]
    fn a_raw_child_leaves_the_terminal_uncooked() {
        let mut session = shell("stty raw -echo; printf 'raw\\r\\n'; exec sleep 10");
        session
            .wait_for(WAIT, |screen| screen.contains("raw"))
            .unwrap();
        assert!(!session.cooked().unwrap());
        session.terminate().unwrap();
        assert_eq!(session.wait_exit(WAIT).unwrap().signal(), Some(15));
    }

    #[test]
    fn stopped_children_can_have_unprocessed_terminal_cleanup() {
        let mut session = shell(
            "read line; printf 'ready\\n'; read line; printf '\\033[2J\\033[H'; kill -STOP $$; printf 'resumed\\n'",
        );
        session.stall_output_after(b"ready\r\n").unwrap();
        session.send(b"start\n");
        session
            .wait_for(WAIT, |screen| screen.contains("ready"))
            .unwrap();
        session.send(b"stop\n");
        assert!(session.wait_until_stopped(WAIT));
        assert!(session.wait_for_pending_output(WAIT));
        assert!(session.screen().contains("ready"));
        session.stall.release();
        session.held_slave = None;
        session
            .wait_for(WAIT, |screen| !screen.contains("ready"))
            .unwrap();
        session.resume().unwrap();
        session
            .wait_for(WAIT, |screen| screen.contains("resumed"))
            .unwrap();
        assert!(session.wait_exit(WAIT).unwrap().success());
        assert!(session.drain_output(WAIT));
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
        let descendant = ready_descendant(&session);
        assert!(
            drops_while_descendant_lives(session, descendant),
            "dropping the session waited for a descendant that holds the terminal"
        );
    }

    #[test]
    fn dropping_a_session_does_not_wait_for_replies_the_child_never_reads() {
        let session = shell(&format!(
            "stty raw -echo; sleep 30 & printf 'descendant %s ready\\r\\n' \"$!\"; {}; \
             printf 'flooded\\r\\n'; wait",
            flood_of_cursor_queries(50_000)
        ));
        let descendant = ready_descendant(&session);
        let flooded = session
            .wait_for(WAIT, |screen| screen.contains("flooded"))
            .is_ok();
        let dropped = drops_while_descendant_lives(session, descendant);
        assert!(
            dropped,
            "dropping the session waited for replies that the child never reads"
        );
        assert!(
            flooded,
            "the reader stopped reading output while replies were held back"
        );
    }

    #[test]
    fn sending_stops_once_the_terminal_side_closes_with_input_unread() {
        let session = Arc::new(shell("stty raw -echo; printf 'ready\\r\\n'; sleep 0.2"));
        session
            .wait_for(WAIT, |screen| screen.contains("ready"))
            .unwrap();
        let (done, finished) = mpsc::channel();
        let sender = Arc::clone(&session);
        thread::spawn(move || {
            sender.send(&vec![b'x'; 1_000_000]);
            let _ = done.send(());
        });
        assert!(
            finished.recv_timeout(WAIT).is_ok(),
            "sending kept retrying input after the terminal side closed"
        );
    }

    #[test]
    fn replies_held_back_by_a_full_input_queue_arrive_whole() {
        let session = shell(&format!(
            "stty raw -echo min 0 time 20; {}; \
             count=$(dd bs=1 count=30000 2>/dev/null | tr '\\033' '\\n' | grep -c '^\\[1;1R$'); \
             stty min 0 time 2; extra=$(dd bs=1 count=1 2>/dev/null | wc -c | tr -d ' '); \
             printf 'replies %s extra %s end\\r\\n' \"$count\" \"$extra\"",
            flood_of_cursor_queries(5_000)
        ));
        let screen = session
            .wait_for(WAIT, |screen| screen.contains(" end"))
            .unwrap();
        assert!(screen.contains("replies 5000 extra 0 end"), "{screen}");
    }
}
