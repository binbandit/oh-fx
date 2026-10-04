use std::env;
use std::ffi::CStr;
use std::ops::Range;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::termios::{
    self, ControlModes, InputModes, LocalModes, OptionalActions, QueueSelector, SpecialCodeIndex,
    Termios,
};

use super::app_lifecycle;
use super::cursor_probe::{CursorPosition, find_position_response, find_position_span};
use super::theme_detection::{THEME_ENV, ThemeDetection, configured_theme, detect_theme_with};
use super::theme_protocol::{
    TerminalBackground, find_osc11_reply, parse_osc11_response, trailing_primary_device_attributes,
    truecolor_supported_for_values,
};
use super::{
    CURSOR_POSITION_QUERY, Layout, THEME_BACKGROUND_QUERY_WITH_FENCE, THEME_COLOR_SCHEME_QUERY,
    THEME_NOTIFICATION_ENABLE_SEQUENCE, THEME_RESPONSE_FENCE_QUERY, TerminalError,
    interactive_mode_enable_sequence,
};

const CURSOR_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
const BACKGROUND_PROBE_TIMEOUT: Duration = Duration::from_millis(200);
const ABNORMAL_RESTORE_WAIT: Duration = Duration::from_millis(100);
const PROBE_REPLY_LIMIT: usize = 64;
const FENCED_BACKGROUND_REPLY_LIMIT: usize = 192;
const CONTROLLING_TERMINAL: &CStr = c"/dev/tty";
const SYNC_UPDATES_ENV: &str = "OH_FX_SYNC_UPDATES";
const LEGACY_SYNC_UPDATES_ENV: &str = "FLASH_SYNC_UPDATES";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryReset {
    EraseScrollback,
    FullReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorSupport {
    Truecolor,
    Palette256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Capabilities {
    pub(crate) tmux: bool,
    pub(crate) sync_updates: bool,
    pub(crate) history_reset: HistoryReset,
    pub(crate) color: ColorSupport,
}

impl Capabilities {
    pub(crate) fn detect() -> Self {
        let tmux = env::var_os("TMUX").is_some();
        let sync_override = env::var(SYNC_UPDATES_ENV)
            .or_else(|_| env::var(LEGACY_SYNC_UPDATES_ENV))
            .ok();
        let term = env::var("TERM").ok();
        let term_program = env::var("TERM_PROGRAM").ok();
        let colorterm = env::var("COLORTERM").ok();
        Self {
            tmux,
            sync_updates: sync_updates_enabled_for_values(
                sync_override.as_deref(),
                term.as_deref(),
            ),
            history_reset: if history_reset_uses_ris_for_values(term_program.as_deref(), tmux) {
                HistoryReset::FullReset
            } else {
                HistoryReset::EraseScrollback
            },
            color: if truecolor_supported_for_values(colorterm.as_deref(), term_program.as_deref())
            {
                ColorSupport::Truecolor
            } else {
                ColorSupport::Palette256
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PollResult {
    pub(crate) readable: bool,
    pub(crate) hung_up: bool,
    pub(crate) has_error: bool,
}

impl PollResult {
    pub(crate) fn closed(self) -> bool {
        self.hung_up || self.has_error
    }
}

pub(crate) struct Terminal {
    input: OwnedFd,
    output: OwnedFd,
    original: Termios,
    raw_enabled: bool,
    capabilities: Capabilities,
    write_abort: Option<OwnedFd>,
    restore_wait: Duration,
    typeahead: Vec<u8>,
}

impl Terminal {
    pub(crate) fn open() -> Result<Self, TerminalError> {
        let stdin = rustix::stdio::stdin();
        let stdout = rustix::stdio::stdout();
        if !termios::isatty(stdin) || !termios::isatty(stdout) {
            return Err(TerminalError::NotATerminal);
        }
        let output = terminal_output(stdout)?;
        Self::from_fds(
            rustix::io::fcntl_dupfd_cloexec(stdin, 0)?,
            output,
            Capabilities::detect(),
        )
    }

    pub(crate) fn abort_writes_when_readable(&mut self, abort: OwnedFd) {
        self.write_abort = Some(abort);
    }

    fn from_fds(
        input: OwnedFd,
        output: OwnedFd,
        capabilities: Capabilities,
    ) -> Result<Self, TerminalError> {
        let original = termios::tcgetattr(&input)?;
        Ok(Self {
            input,
            output,
            original,
            raw_enabled: false,
            capabilities,
            write_abort: None,
            restore_wait: ABNORMAL_RESTORE_WAIT,
            typeahead: Vec::new(),
        })
    }

    pub(crate) fn input_fd(&self) -> BorrowedFd<'_> {
        self.input.as_fd()
    }

    pub(crate) fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    pub(crate) fn capture_original_termios(&mut self) -> Result<(), TerminalError> {
        self.original = termios::tcgetattr(&self.input)?;
        Ok(())
    }

    pub(crate) fn enable_raw_mode(&mut self) -> Result<(), TerminalError> {
        termios::tcsetattr(
            &self.input,
            OptionalActions::Now,
            &raw_termios(&self.original),
        )?;
        self.raw_enabled = true;
        Ok(())
    }

    pub(crate) fn raw_mode_lost(&self) -> Result<bool, TerminalError> {
        if !self.raw_enabled {
            return Ok(false);
        }
        let current = termios::tcgetattr(&self.input)?;
        Ok(current
            .local_modes
            .intersects(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG))
    }

    pub(crate) fn disable_raw_mode(&mut self) {
        if self.raw_enabled {
            let _ = termios::tcsetattr(&self.input, OptionalActions::Flush, &self.original);
            self.raw_enabled = false;
        }
    }

    pub(crate) fn restore_abnormally(&mut self) {
        let was_raw = std::mem::take(&mut self.raw_enabled);
        if was_raw {
            let _ = termios::tcsetattr(&self.input, OptionalActions::Now, &self.original);
        }
        self.write_abnormal_restore();
        if was_raw {
            let _ = termios::tcflush(&self.input, QueueSelector::IFlush);
        }
    }

    pub(crate) fn query_layout(&self, footer_rows: u16) -> Result<Layout, TerminalError> {
        let size = termios::tcgetwinsize(&self.input)
            .map_err(|_| TerminalError::UnableToReadTerminalSize)?;
        if size.ws_row == 0 || size.ws_col == 0 {
            return Err(TerminalError::UnableToReadTerminalSize);
        }
        Layout::from_size(size.ws_row, size.ws_col, footer_rows)
    }

    pub(crate) fn query_cursor_position(&mut self) -> Result<CursorPosition, TerminalError> {
        self.write_all(CURSOR_POSITION_QUERY.as_bytes())?;
        let reply = self.read_reply(CURSOR_PROBE_TIMEOUT, PROBE_REPLY_LIMIT, |bytes| {
            find_position_span(bytes).map(|(span, _)| span)
        })?;
        reply
            .and_then(|reply| find_position_response(&reply))
            .ok_or(TerminalError::CursorPositionUnavailable)
    }

    pub(crate) fn query_background(&mut self) -> Option<TerminalBackground> {
        self.write_all(THEME_BACKGROUND_QUERY_WITH_FENCE.as_bytes())
            .ok()?;
        let unclaimed = self.typeahead.len();
        let fenced = self.read_reply(
            BACKGROUND_PROBE_TIMEOUT,
            FENCED_BACKGROUND_REPLY_LIMIT,
            |bytes| trailing_primary_device_attributes(bytes).map(|start| start..bytes.len()),
        );
        let span = find_osc11_reply(&self.typeahead[unclaimed..])
            .map(|span| span.start + unclaimed..span.end + unclaimed);
        let reply: Option<Vec<u8>> = span.map(|span| self.typeahead.drain(span).collect());
        fenced.ok()?;
        parse_osc11_response(&reply?)
    }

    pub(crate) fn take_typeahead(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.typeahead)
    }

    pub(crate) fn detect_theme(&mut self, setting: Option<&str>) -> ThemeDetection {
        let environment = env::var(THEME_ENV).ok();
        let colorfgbg = env::var("COLORFGBG").ok();
        detect_theme_with(
            configured_theme(environment.as_deref(), setting),
            || self.query_background(),
            colorfgbg.as_deref(),
        )
    }

    pub(crate) fn enter_interactive_mode(&self) -> Result<(), TerminalError> {
        self.write_all(interactive_mode_enable_sequence(self.capabilities.tmux).as_bytes())
    }

    pub(crate) fn enable_theme_notifications(&self) -> Result<(), TerminalError> {
        self.write_all(THEME_NOTIFICATION_ENABLE_SEQUENCE.as_bytes())
    }

    pub(crate) fn request_theme_color_scheme(&self) -> Result<(), TerminalError> {
        self.write_all(THEME_COLOR_SCHEME_QUERY.as_bytes())
    }

    pub(crate) fn request_theme_response_fence(&self) -> Result<(), TerminalError> {
        self.write_all(THEME_RESPONSE_FENCE_QUERY.as_bytes())
    }

    pub(crate) fn request_theme_background(&self) -> Result<(), TerminalError> {
        self.write_all(THEME_BACKGROUND_QUERY_WITH_FENCE.as_bytes())
    }

    pub(crate) fn poll_input(
        &self,
        timeout: Option<Duration>,
    ) -> Result<PollResult, TerminalError> {
        poll_retrying_interrupts(timeout, |timeout| {
            let mut fds = [PollFd::new(&self.input, PollFlags::IN)];
            rustix::event::poll(&mut fds, timeout)?;
            let revents = fds[0].revents();
            Ok(PollResult {
                readable: revents.contains(PollFlags::IN),
                hung_up: revents.contains(PollFlags::HUP),
                has_error: revents.contains(PollFlags::ERR),
            })
        })
    }

    pub(crate) fn read(&self, buffer: &mut [u8]) -> Result<usize, TerminalError> {
        loop {
            match rustix::io::read(&self.input, &mut *buffer) {
                Ok(count) => return Ok(count),
                Err(Errno::INTR) => {}
                Err(errno) => return Err(errno.into()),
            }
        }
    }

    pub(crate) fn write_all(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        let abort = self.write_abort.as_ref().map(AsFd::as_fd);
        write_fully(self.output.as_fd(), bytes, abort, None).map_err(TerminalError::from)
    }

    fn write_abnormal_restore(&self) {
        let output = self.output.as_fd();
        let deadline = Some(Instant::now() + self.restore_wait);
        let _ = app_lifecycle::abnormal_exit_restore_sequences(self.capabilities.tmux)
            .try_for_each(|sequence| {
                wait_until_writable(output, None, deadline)?;
                write_fully(output, sequence.as_bytes(), None, deadline)
            });
    }

    fn read_reply(
        &mut self,
        timeout: Duration,
        limit: usize,
        find: impl Fn(&[u8]) -> Option<Range<usize>>,
    ) -> Result<Option<Vec<u8>>, TerminalError> {
        let deadline = Instant::now() + timeout;
        let mut received = Vec::with_capacity(limit);
        let mut reply = None;
        while received.len() < limit {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let poll = self.poll_input(Some(remaining))?;
            if poll.closed() || !poll.readable {
                break;
            }
            let mut byte = [0_u8];
            if self.read(&mut byte)? == 0 {
                break;
            }
            received.push(byte[0]);
            if let Some(span) = find(&received) {
                reply = Some(received.drain(span).collect());
                break;
            }
        }
        self.typeahead.extend_from_slice(&received);
        Ok(reply)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.raw_enabled {
            self.restore_abnormally();
        }
    }
}

fn poll_retrying_interrupts(
    timeout: Option<Duration>,
    mut poll_once: impl FnMut(Option<&Timespec>) -> Result<PollResult, Errno>,
) -> Result<PollResult, TerminalError> {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    loop {
        let remaining = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .and_then(|remaining| Timespec::try_from(remaining).ok());
        match poll_once(remaining.as_ref()) {
            Err(Errno::INTR) => {}
            result => return result.map_err(TerminalError::from),
        }
    }
}

fn terminal_output(stdout: BorrowedFd<'_>) -> rustix::io::Result<OwnedFd> {
    let flags = OFlags::WRONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let reopen = |path: &CStr| {
        rustix::fs::open(path, flags, Mode::empty())
            .ok()
            .filter(pollable)
    };
    if let Some(output) = termios::ttyname(stdout, Vec::new())
        .ok()
        .and_then(|path| reopen(&path))
    {
        return Ok(output);
    }
    if controls_this_session(stdout)
        && let Some(output) = reopen(CONTROLLING_TERMINAL)
    {
        return Ok(output);
    }
    rustix::io::fcntl_dupfd_cloexec(stdout, 0)
}

fn pollable(output: &OwnedFd) -> bool {
    let mut fds = [PollFd::new(output, PollFlags::OUT)];
    rustix::event::poll(&mut fds, Some(&Timespec::default())).is_ok()
        && !fds[0].revents().contains(PollFlags::NVAL)
}

fn controls_this_session(terminal: BorrowedFd<'_>) -> bool {
    matches!(
        (termios::tcgetsid(terminal), rustix::process::getsid(None)),
        (Ok(terminal_session), Ok(session)) if terminal_session == session
    )
}

fn write_fully(
    fd: BorrowedFd<'_>,
    mut bytes: &[u8],
    abort: Option<BorrowedFd<'_>>,
    deadline: Option<Instant>,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => wait_until_writable(fd, abort, deadline)?,
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(())
}

fn wait_until_writable(
    fd: BorrowedFd<'_>,
    abort: Option<BorrowedFd<'_>>,
    deadline: Option<Instant>,
) -> std::io::Result<()> {
    let watched = abort.unwrap_or(fd);
    let abort_events = if abort.is_some() {
        PollFlags::IN
    } else {
        PollFlags::empty()
    };
    loop {
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let timeout = remaining.and_then(|remaining| Timespec::try_from(remaining).ok());
        let mut fds = [
            PollFd::new(&fd, PollFlags::OUT),
            PollFd::new(&watched, abort_events),
        ];
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(errno) => return Err(errno.into()),
        }
        let aborted = if abort.is_some() {
            fds[1].revents()
        } else {
            PollFlags::empty()
        };
        if let Some(ready) = write_readiness(fds[0].revents(), aborted) {
            return ready;
        }
    }
}

fn write_readiness(output: PollFlags, abort: PollFlags) -> Option<std::io::Result<()>> {
    if abort.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
        Some(Err(std::io::ErrorKind::Interrupted.into()))
    } else if output.union(abort).contains(PollFlags::NVAL) {
        Some(Err(Errno::BADF.into()))
    } else if output.is_empty() {
        None
    } else {
        Some(Ok(()))
    }
}

fn raw_termios(original: &Termios) -> Termios {
    let mut raw = original.clone();
    raw.input_modes.remove(
        InputModes::BRKINT
            | InputModes::IGNCR
            | InputModes::ICRNL
            | InputModes::INLCR
            | InputModes::INPCK
            | InputModes::ISTRIP
            | InputModes::IXON
            | InputModes::IXOFF,
    );
    raw.control_modes.remove(ControlModes::CSIZE);
    raw.control_modes.insert(ControlModes::CS8);
    raw.local_modes
        .remove(LocalModes::ECHO | LocalModes::ICANON | LocalModes::IEXTEN | LocalModes::ISIG);
    raw.special_codes[SpecialCodeIndex::VMIN] = 1;
    raw.special_codes[SpecialCodeIndex::VTIME] = 0;
    raw
}

fn sync_updates_enabled_for_values(sync_override: Option<&str>, term: Option<&str>) -> bool {
    if let Some(value) = sync_override {
        if ["0", "false", "off"]
            .iter()
            .any(|candidate| value.eq_ignore_ascii_case(candidate))
        {
            return false;
        }
        if ["1", "true", "on"]
            .iter()
            .any(|candidate| value.eq_ignore_ascii_case(candidate))
        {
            return true;
        }
    }
    term != Some("dumb")
}

fn history_reset_uses_ris_for_values(term_program: Option<&str>, tmux: bool) -> bool {
    !tmux && term_program == Some("Apple_Terminal")
}

#[cfg(test)]
pub(crate) mod test_pty {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    use ofx_testkit::{PtyPair, PtySession};
    use rustix::termios::LocalModes;

    use super::{Capabilities, ColorSupport, HistoryReset, Terminal};

    const CHILD: &str = "OH_FX_TERMINAL_CHILD";
    const AMBIENT: [&str; 8] = [
        "TMUX",
        "TERM",
        "TERM_PROGRAM",
        "COLORTERM",
        "COLORFGBG",
        "OH_FX_THEME",
        "OH_FX_SYNC_UPDATES",
        "FLASH_SYNC_UPDATES",
    ];
    pub(crate) const WAIT: Duration = Duration::from_secs(10);

    pub(crate) fn open() -> PtyPair {
        PtyPair::open(24, 80).unwrap()
    }

    pub(crate) fn spawn_on(pty: &PtyPair, test: &str) -> Child {
        child_command(test, &[])
            .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
            .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    pub(crate) fn exit_within(mut child: Child, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        None
    }

    pub(crate) fn cooked(modes: LocalModes) -> bool {
        modes.contains(LocalModes::ICANON | LocalModes::ECHO)
    }

    pub(crate) fn in_child() -> bool {
        std::env::var_os(CHILD).is_some()
    }

    pub(crate) fn child_command(test: &str, env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1");
        for name in AMBIENT {
            command.env_remove(name);
        }
        command.envs(env.iter().copied());
        command
    }

    pub(crate) fn child_session(test: &str, env: &[(&str, &str)]) -> PtySession {
        let mut command = child_command(test, env);
        command.process_group(0);
        PtySession::spawn(command, 24, 80).unwrap()
    }

    pub(crate) fn wait_output(session: &PtySession, needle: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + WAIT;
        loop {
            let output = session.output();
            if output.windows(needle.len()).any(|window| window == needle) {
                return output;
            }
            assert!(
                Instant::now() < deadline,
                "expected {:?} in {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&output)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub(crate) fn type_ahead(pty: &PtyPair, terminal: &Terminal) {
        rustix::io::write(&pty.master, b"git push --force\r").unwrap();
        assert!(terminal.poll_input(Some(WAIT)).unwrap().readable);
    }

    pub(crate) fn unread_input(pty: &PtyPair) -> u64 {
        rustix::io::ioctl_fionread(&pty.slave).unwrap()
    }

    pub(crate) fn terminal(pty: &PtyPair) -> Terminal {
        Terminal::from_fds(
            pty.slave.try_clone().unwrap(),
            pty.slave.try_clone().unwrap(),
            Capabilities {
                tmux: false,
                sync_updates: true,
                history_reset: HistoryReset::EraseScrollback,
                color: ColorSupport::Truecolor,
            },
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use std::time::Duration;

    use rustix::termios::{self, InputModes, LocalModes, OptionalActions, SpecialCodeIndex};

    use ofx_testkit::PtyPair;

    use super::test_pty;
    use super::*;

    fn read_master(pty: &PtyPair) -> Vec<u8> {
        let mut collected = Vec::new();
        let mut buffer = [0_u8; 256];
        loop {
            let mut fds = [PollFd::new(&pty.master, PollFlags::IN)];
            let timeout = Timespec::try_from(Duration::from_millis(50)).unwrap();
            if rustix::event::poll(&mut fds, Some(&timeout)).unwrap() == 0 {
                return collected;
            }
            let count = rustix::io::read(&pty.master, &mut buffer).unwrap();
            if count == 0 {
                return collected;
            }
            collected.extend_from_slice(&buffer[..count]);
        }
    }

    #[test]
    fn interrupted_polls_retry_with_the_remaining_deadline() {
        let mut timeouts = Vec::new();
        let result = poll_retrying_interrupts(Some(Duration::from_secs(5)), |timeout| {
            timeouts.push(timeout.copied());
            if timeouts.len() == 1 {
                Err(Errno::INTR)
            } else {
                Ok(PollResult {
                    readable: true,
                    ..PollResult::default()
                })
            }
        })
        .unwrap();
        assert!(result.readable);
        assert_eq!(timeouts.len(), 2);
        let first = timeouts[0].unwrap();
        let second = timeouts[1].unwrap();
        assert!(second.tv_sec < first.tv_sec || second.tv_nsec <= first.tv_nsec);
        let mut calls = 0;
        let unbounded = poll_retrying_interrupts(None, |timeout| {
            calls += 1;
            assert!(timeout.is_none());
            if calls < 3 {
                Err(Errno::INTR)
            } else {
                Err(Errno::BADF)
            }
        });
        assert!(unbounded.is_err());
        assert_eq!(calls, 3);
    }

    fn nonblocking_terminal(pty: &PtyPair) -> Terminal {
        let name = rustix::pty::ptsname(&pty.master, Vec::new()).unwrap();
        let flags = OFlags::RDWR | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
        let output = rustix::fs::open(name.as_c_str(), flags, Mode::empty()).unwrap();
        Terminal::from_fds(
            pty.slave.try_clone().unwrap(),
            output,
            test_pty::terminal(pty).capabilities(),
        )
        .unwrap()
    }

    #[test]
    fn a_blocked_write_ends_when_the_abort_descriptor_wakes() {
        let pty = test_pty::open();
        let mut terminal = nonblocking_terminal(&pty);
        let (abort, wake) = std::os::unix::net::UnixStream::pair().unwrap();
        terminal.abort_writes_when_readable(abort.into());
        rustix::io::write(&wake, &[1]).unwrap();
        let started = Instant::now();
        let error = terminal.write_all(&vec![b'x'; 1 << 22]).unwrap_err();
        assert!(matches!(
            error,
            TerminalError::Io(ref io) if io.kind() == std::io::ErrorKind::Interrupted
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!read_master(&pty).is_empty());
    }

    fn abnormal_restore() -> String {
        app_lifecycle::abnormal_exit_restore_sequences(false).collect()
    }

    fn drain_until(master: &OwnedFd, needle: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + test_pty::WAIT;
        let mut collected = Vec::new();
        let mut buffer = [0_u8; 8192];
        while Instant::now() < deadline
            && !collected
                .windows(needle.len())
                .any(|window| window == needle)
        {
            let mut fds = [PollFd::new(master, PollFlags::IN)];
            let timeout = Timespec::try_from(Duration::from_millis(50)).unwrap();
            if rustix::event::poll(&mut fds, Some(&timeout)).unwrap() > 0 {
                let count = rustix::io::read(master, &mut buffer).unwrap();
                collected.extend_from_slice(&buffer[..count]);
            }
        }
        collected
    }

    fn fill_output_queue(terminal: &Terminal) {
        let chunk = [b'x'; 4096];
        loop {
            while rustix::io::write(&terminal.output, &chunk).is_ok() {}
            let mut fds = [PollFd::new(&terminal.output, PollFlags::OUT)];
            let settle = Timespec::try_from(Duration::from_millis(50)).unwrap();
            if rustix::event::poll(&mut fds, Some(&settle)).unwrap() == 0 {
                return;
            }
        }
    }

    #[test]
    fn the_signal_restore_waits_for_a_full_queue_after_the_abort_descriptor_wakes() {
        let pty = test_pty::open();
        let mut terminal = nonblocking_terminal(&pty);
        let (abort, wake) = std::os::unix::net::UnixStream::pair().unwrap();
        terminal.abort_writes_when_readable(abort.into());
        rustix::io::write(&wake, &[1]).unwrap();
        terminal.enable_raw_mode().unwrap();
        fill_output_queue(&terminal);
        let started = Instant::now();
        terminal.restore_abnormally();
        assert!(started.elapsed() >= ABNORMAL_RESTORE_WAIT);
        terminal.restore_wait = test_pty::WAIT;
        let restore = abnormal_restore().replace('\n', "\r\n");
        let master = pty.master.try_clone().unwrap();
        let needle = restore.clone().into_bytes();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            drain_until(&master, &needle)
        });
        terminal.restore_abnormally();
        let written = reader.join().unwrap();
        assert!(
            written.ends_with(restore.as_bytes()),
            "{:?}",
            String::from_utf8_lossy(&written[written.len().saturating_sub(200)..])
        );
    }

    #[test]
    fn the_signal_restore_discards_input_typed_while_it_waits_for_the_terminal() {
        let pty = test_pty::open();
        let mut quiet = termios::tcgetattr(&pty.slave).unwrap();
        quiet.local_modes.remove(LocalModes::ECHO);
        termios::tcsetattr(&pty.slave, OptionalActions::Now, &quiet).unwrap();
        let mut terminal = nonblocking_terminal(&pty);
        terminal.restore_wait = test_pty::WAIT;
        terminal.enable_raw_mode().unwrap();
        fill_output_queue(&terminal);
        let master = pty.master.try_clone().unwrap();
        let slave = pty.slave.try_clone().unwrap();
        let restore = abnormal_restore().replace('\n', "\r\n").into_bytes();
        let needle = restore.clone();
        let typist = std::thread::spawn(move || {
            let deadline = Instant::now() + test_pty::WAIT;
            while !termios::tcgetattr(&slave)
                .unwrap()
                .local_modes
                .contains(LocalModes::ICANON)
            {
                assert!(Instant::now() < deadline, "termios never became canonical");
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(20));
            rustix::io::write(&master, b"git push --force\r").unwrap();
            let mut fds = [PollFd::new(&slave, PollFlags::IN)];
            let wait = Timespec::try_from(test_pty::WAIT).unwrap();
            assert_eq!(rustix::event::poll(&mut fds, Some(&wait)).unwrap(), 1);
            drain_until(&master, &needle)
        });
        terminal.restore_abnormally();
        assert!(typist.join().unwrap().ends_with(&restore));
        assert_eq!(test_pty::unread_input(&pty), 0);
    }

    #[test]
    fn an_abnormal_restore_outside_raw_mode_keeps_unread_input() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        rustix::io::write(&pty.master, b"typed\r").unwrap();
        assert!(terminal.poll_input(Some(test_pty::WAIT)).unwrap().readable);
        terminal.restore_abnormally();
        assert_eq!(test_pty::unread_input(&pty), 6);
    }

    #[test]
    fn nonblocking_writes_wait_for_a_slow_reader_without_losing_bytes() {
        let pty = test_pty::open();
        let mut terminal = nonblocking_terminal(&pty);
        let (abort, _wake) = std::os::unix::net::UnixStream::pair().unwrap();
        terminal.abort_writes_when_readable(abort.into());
        let total = 1 << 20;
        let master = pty.master.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut received = 0;
            let mut buffer = [0_u8; 8192];
            while received < total {
                std::thread::sleep(Duration::from_millis(1));
                received += rustix::io::read(&master, &mut buffer).unwrap();
            }
            received
        });
        terminal.write_all(&vec![b'y'; total]).unwrap();
        assert_eq!(reader.join().unwrap(), total);
    }

    #[test]
    fn write_waits_end_on_room_errors_or_an_abort_but_not_on_an_unpollable_descriptor() {
        let quiet = PollFlags::empty();
        assert!(write_readiness(quiet, quiet).is_none());
        for output in [PollFlags::OUT, PollFlags::HUP, PollFlags::ERR] {
            assert!(matches!(write_readiness(output, quiet), Some(Ok(()))));
        }
        for (output, abort) in [(PollFlags::NVAL, quiet), (PollFlags::OUT, PollFlags::NVAL)] {
            let error = write_readiness(output, abort).unwrap().unwrap_err();
            assert_eq!(error.raw_os_error(), Some(Errno::BADF.raw_os_error()));
        }
        for abort in [PollFlags::IN, PollFlags::HUP, PollFlags::ERR] {
            for output in [quiet, PollFlags::OUT, PollFlags::NVAL] {
                let error = write_readiness(output, abort).unwrap().unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
            }
        }
    }

    #[test]
    fn sync_updates_override_beats_dumb_term() {
        assert!(sync_updates_enabled_for_values(Some("on"), Some("dumb")));
        assert!(!sync_updates_enabled_for_values(
            Some("off"),
            Some("xterm-256color")
        ));
        assert!(!sync_updates_enabled_for_values(None, Some("dumb")));
    }

    #[test]
    fn direct_apple_terminal_uses_ris_for_terminal_history_resets() {
        assert!(history_reset_uses_ris_for_values(
            Some("Apple_Terminal"),
            false
        ));
        assert!(!history_reset_uses_ris_for_values(
            Some("Apple_Terminal"),
            true
        ));
        assert!(!history_reset_uses_ris_for_values(Some("Ghostty"), false));
    }

    #[test]
    fn enable_raw_mode_preserves_already_queued_input() {
        let pty = test_pty::open();
        let mut original = termios::tcgetattr(&pty.slave).unwrap();
        original
            .local_modes
            .remove(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG);
        original.special_codes[SpecialCodeIndex::VMIN] = 1;
        original.special_codes[SpecialCodeIndex::VTIME] = 0;
        termios::tcsetattr(&pty.slave, OptionalActions::Now, &original).unwrap();

        let mut terminal = test_pty::terminal(&pty);
        rustix::io::write(&pty.master, &[3]).unwrap();
        terminal.enable_raw_mode().unwrap();

        let poll = terminal
            .poll_input(Some(Duration::from_millis(100)))
            .unwrap();
        assert!(poll.readable);
        let mut byte = [0_u8];
        assert_eq!(terminal.read(&mut byte).unwrap(), 1);
        assert_eq!(byte[0], 3);
        terminal.disable_raw_mode();
    }

    #[test]
    fn enable_raw_mode_preserves_carriage_return_input() {
        let pty = test_pty::open();
        let mut original = termios::tcgetattr(&pty.slave).unwrap();
        original
            .input_modes
            .insert(InputModes::IGNCR | InputModes::ICRNL | InputModes::INLCR);
        termios::tcsetattr(&pty.slave, OptionalActions::Now, &original).unwrap();

        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        let raw = termios::tcgetattr(&pty.slave).unwrap();
        assert!(!raw.input_modes.contains(InputModes::IGNCR));
        assert!(!raw.input_modes.contains(InputModes::ICRNL));
        assert!(!raw.input_modes.contains(InputModes::INLCR));

        rustix::io::write(&pty.master, b"\r").unwrap();
        let poll = terminal
            .poll_input(Some(Duration::from_millis(100)))
            .unwrap();
        assert!(poll.readable);
        let mut byte = [0_u8];
        assert_eq!(terminal.read(&mut byte).unwrap(), 1);
        assert_eq!(byte[0], b'\r');
        terminal.disable_raw_mode();
    }

    #[test]
    fn raw_mode_disables_echo_canonical_input_and_signals() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        let raw = termios::tcgetattr(&pty.slave).unwrap();
        assert!(!raw.local_modes.intersects(
            LocalModes::ECHO | LocalModes::ICANON | LocalModes::IEXTEN | LocalModes::ISIG
        ));
        assert_eq!(raw.special_codes[SpecialCodeIndex::VMIN], 1);
        assert_eq!(raw.special_codes[SpecialCodeIndex::VTIME], 0);

        terminal.disable_raw_mode();
        let restored = termios::tcgetattr(&pty.slave).unwrap();
        assert!(restored.local_modes.contains(LocalModes::ICANON));
    }

    #[test]
    fn dropping_an_armed_terminal_restores_modes_and_termios() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        drop(terminal);

        let restored = termios::tcgetattr(&pty.slave).unwrap();
        assert!(restored.local_modes.contains(LocalModes::ICANON));
        let written = String::from_utf8(read_master(&pty)).unwrap();
        assert!(written.replace("\r\n", "\n").ends_with(&abnormal_restore()));
    }

    #[test]
    fn caught_panics_leave_the_terminal_raw() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        let caught = std::panic::catch_unwind(|| panic!("recoverable"));
        assert!(caught.is_err());
        assert!(terminal.raw_enabled);
        let current = termios::tcgetattr(&pty.slave).unwrap();
        assert!(!current.local_modes.contains(LocalModes::ICANON));
        assert!(read_master(&pty).is_empty());
        terminal.disable_raw_mode();
    }

    #[test]
    fn unwinding_through_the_owner_restores_the_terminal() {
        let pty = test_pty::open();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut terminal = test_pty::terminal(&pty);
            terminal.enable_raw_mode().unwrap();
            panic!("fatal");
        }));
        assert!(unwound.is_err());
        let restored = termios::tcgetattr(&pty.slave).unwrap();
        assert!(restored.local_modes.contains(LocalModes::ICANON));
        let written = String::from_utf8(read_master(&pty)).unwrap();
        assert!(written.replace("\r\n", "\n").ends_with(&abnormal_restore()));
    }

    #[test]
    fn unwinding_through_the_owner_discards_input_typed_for_it() {
        let pty = test_pty::open();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut terminal = test_pty::terminal(&pty);
            terminal.enable_raw_mode().unwrap();
            test_pty::type_ahead(&pty, &terminal);
            panic!("fatal");
        }));
        assert!(unwound.is_err());
        let restored = termios::tcgetattr(&pty.slave).unwrap();
        assert!(restored.local_modes.contains(LocalModes::ICANON));
        assert_eq!(test_pty::unread_input(&pty), 0);
    }

    #[test]
    fn a_startup_error_after_raw_mode_discards_input_typed_for_the_session() {
        let pty = test_pty::open();
        let empty = termios::Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        termios::tcsetwinsize(&pty.master, empty).unwrap();
        let start = || -> Result<Terminal, TerminalError> {
            let mut terminal = test_pty::terminal(&pty);
            terminal.enable_raw_mode()?;
            test_pty::type_ahead(&pty, &terminal);
            terminal.query_layout(4)?;
            Ok(terminal)
        };
        assert!(matches!(
            start(),
            Err(TerminalError::UnableToReadTerminalSize)
        ));
        let restored = termios::tcgetattr(&pty.slave).unwrap();
        assert!(restored.local_modes.contains(LocalModes::ICANON));
        assert_eq!(test_pty::unread_input(&pty), 0);
    }

    fn forbid_reopening_by_path() {
        let stdout = rustix::stdio::stdout();
        rustix::fs::fchmod(stdout, Mode::empty()).unwrap();
        #[cfg(target_os = "linux")]
        {
            let mut sets = rustix::thread::capabilities(None).unwrap();
            sets.effective = rustix::thread::CapabilitySet::empty();
            rustix::thread::set_capabilities(None, sets).unwrap();
        }
        let path = termios::ttyname(stdout, Vec::new()).unwrap();
        let flags = OFlags::WRONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
        assert_eq!(
            rustix::fs::open(path.as_c_str(), flags, Mode::empty()).err(),
            Some(Errno::ACCESS)
        );
    }

    #[test]
    fn unwinding_with_stalled_output_restores_termios_without_waiting_for_it() {
        if test_pty::in_child() {
            let unwound = std::panic::catch_unwind(|| {
                let mut terminal = Terminal::open().unwrap();
                terminal.enable_raw_mode().unwrap();
                let chunk = [b'x'; 4096];
                while rustix::io::write(&terminal.output, &chunk).is_ok() {}
                panic!("unwinding with stalled output");
            });
            std::process::exit(i32::from(unwound.is_ok()));
        }
        let pty = test_pty::open();
        let child = test_pty::spawn_on(
            &pty,
            "terminal::shell_runtime::tests::unwinding_with_stalled_output_restores_termios_without_waiting_for_it",
        );
        let status = test_pty::exit_within(child, test_pty::WAIT);
        let modes = termios::tcgetattr(&pty.slave).unwrap().local_modes;
        assert!(
            status.is_some_and(|status| status.success()) && test_pty::cooked(modes),
            "{status:?} {modes:?}"
        );
    }

    fn stall_blocking_output() {
        std::thread::spawn(|| {
            let chunk = [b'x'; 4096];
            while rustix::io::write(rustix::stdio::stdout(), &chunk).is_ok() {}
        });
        let stdout = rustix::stdio::stdout();
        let mut fds = [PollFd::new(&stdout, PollFlags::OUT)];
        let settle = Timespec::try_from(Duration::from_millis(50)).unwrap();
        while rustix::event::poll(&mut fds, Some(&settle)).unwrap() > 0 {}
    }

    #[test]
    fn unwinding_on_blocking_output_gives_up_on_a_stalled_terminal() {
        if test_pty::in_child() {
            forbid_reopening_by_path();
            let mut terminal = Terminal::open().unwrap();
            stall_blocking_output();
            let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                terminal.enable_raw_mode().unwrap();
                let _owner = terminal;
                panic!("unwinding with stalled blocking output");
            }));
            std::process::exit(i32::from(unwound.is_ok()));
        }
        let pty = test_pty::open();
        let child = test_pty::spawn_on(
            &pty,
            "terminal::shell_runtime::tests::unwinding_on_blocking_output_gives_up_on_a_stalled_terminal",
        );
        let status = test_pty::exit_within(child, test_pty::WAIT);
        let modes = termios::tcgetattr(&pty.slave).unwrap().local_modes;
        assert!(
            status.is_some_and(|status| status.success()) && test_pty::cooked(modes),
            "{status:?} {modes:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sigterm_ends_a_stalled_write_when_the_terminal_path_cannot_be_reopened() {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::ExitStatusExt;
        use std::path::Path;

        use rustix::process::{Pid, Signal};

        use super::super::signal_pipe::{SignalPipe, raise_default};

        if test_pty::in_child() {
            rustix::process::setsid().unwrap();
            rustix::process::ioctl_tiocsctty(rustix::stdio::stdin()).unwrap();
            forbid_reopening_by_path();
            let mut terminal = Terminal::open().unwrap();
            let output = format!("/proc/self/fd/{}", terminal.output.as_raw_fd());
            assert_eq!(std::fs::read_link(output).unwrap(), Path::new("/dev/tty"));
            let mut signals = SignalPipe::install().unwrap();
            terminal.abort_writes_when_readable(signals.fatal_wakeup().unwrap());
            terminal.enable_raw_mode().unwrap();
            assert!(terminal.write_all(&vec![b'x'; 1 << 22]).is_err());
            let signal = signals.take().fatal.unwrap();
            terminal.restore_abnormally();
            drop(terminal);
            signals.uninstall();
            raise_default(signal);
            std::process::exit(1);
        }
        let pty = test_pty::open();
        let mut child = test_pty::spawn_on(
            &pty,
            "terminal::shell_runtime::tests::sigterm_ends_a_stalled_write_when_the_terminal_path_cannot_be_reopened",
        );
        let deadline = Instant::now() + test_pty::WAIT;
        while Instant::now() < deadline
            && child.try_wait().unwrap().is_none()
            && termios::tcgetattr(&pty.slave)
                .unwrap()
                .local_modes
                .contains(LocalModes::ICANON)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        let _ = rustix::process::kill_process(Pid::from_child(&child), Signal::TERM);
        let status = test_pty::exit_within(child, test_pty::WAIT);
        let modes = termios::tcgetattr(&pty.slave).unwrap().local_modes;
        assert!(
            status.and_then(|status| status.signal()) == Some(Signal::TERM.as_raw())
                && test_pty::cooked(modes),
            "{status:?} {modes:?}"
        );
    }

    #[test]
    fn opening_falls_back_to_blocking_output_when_the_terminal_cannot_be_reopened() {
        if test_pty::in_child() {
            forbid_reopening_by_path();
            let mut terminal = Terminal::open().unwrap();
            let flags = rustix::fs::fcntl_getfl(&terminal.output).unwrap();
            let output = rustix::fs::fstat(&terminal.output).unwrap().st_rdev;
            let stdout = rustix::fs::fstat(rustix::stdio::stdout()).unwrap().st_rdev;
            terminal.enable_raw_mode().unwrap();
            let line = format!(
                "fallback blocking={} same={}\r\n",
                !flags.contains(OFlags::NONBLOCK),
                output == stdout
            );
            terminal.write_all(line.as_bytes()).unwrap();
            drop(terminal);
            return;
        }
        let mut session = test_pty::child_session(
            "terminal::shell_runtime::tests::opening_falls_back_to_blocking_output_when_the_terminal_cannot_be_reopened",
            &[],
        );
        let restore = abnormal_restore().replace('\n', "\r\n");
        let output = test_pty::wait_output(&session, restore.as_bytes());
        let reported = b"fallback blocking=true same=true";
        assert!(
            output
                .windows(reported.len())
                .any(|window| window == reported)
        );
        assert!(session.wait_exit(test_pty::WAIT).unwrap().success());
    }

    #[test]
    fn disabled_raw_mode_leaves_nothing_for_drop_to_restore() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        terminal.disable_raw_mode();
        drop(terminal);
        assert!(read_master(&pty).is_empty());
        let restored = termios::tcgetattr(&pty.slave).unwrap();
        assert!(restored.local_modes.contains(LocalModes::ICANON));
    }

    #[test]
    fn cursor_position_query_reads_the_terminal_reply() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        rustix::io::write(&pty.master, b"\x1b[7;3R").unwrap();
        assert_eq!(
            terminal.query_cursor_position().unwrap(),
            CursorPosition { row: 7, col: 3 }
        );
        assert!(read_master(&pty).starts_with(CURSOR_POSITION_QUERY.as_bytes()));
        terminal.disable_raw_mode();
    }

    #[test]
    fn probe_replies_hand_surrounding_typeahead_back_as_input() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        rustix::io::write(&pty.master, b"ab\x1b]11;rgb:0000/0000/0000\x07\x1b[?1;2c").unwrap();
        assert!(!terminal.query_background().unwrap().light);
        rustix::io::write(&pty.master, b"cd\x1b[7;3Re").unwrap();
        assert_eq!(
            terminal.query_cursor_position().unwrap(),
            CursorPosition { row: 7, col: 3 }
        );
        assert_eq!(terminal.take_typeahead(), b"abcd");
        rustix::io::write(&pty.master, b"typed").unwrap();
        assert!(terminal.query_cursor_position().is_err());
        assert_eq!(terminal.take_typeahead(), b"etyped");
        assert!(terminal.take_typeahead().is_empty());
        assert_eq!(
            read_master(&pty),
            [
                THEME_BACKGROUND_QUERY_WITH_FENCE,
                CURSOR_POSITION_QUERY,
                CURSOR_POSITION_QUERY
            ]
            .concat()
            .as_bytes()
        );
        terminal.disable_raw_mode();
    }

    #[test]
    fn background_query_parses_the_osc_11_reply_and_consumes_its_fence() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        rustix::io::write(&pty.master, b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;22c").unwrap();
        let background = terminal.query_background().unwrap();
        assert!(background.light);
        assert!(terminal.take_typeahead().is_empty());
        assert_eq!(
            read_master(&pty),
            THEME_BACKGROUND_QUERY_WITH_FENCE.as_bytes()
        );
        terminal.disable_raw_mode();
    }

    #[test]
    fn a_terminal_that_ignores_osc_11_ends_the_probe_at_the_fence() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        rustix::io::write(&pty.master, b"\x1b[?62;22ctyped").unwrap();
        assert_eq!(terminal.query_background(), None);
        assert!(terminal.take_typeahead().is_empty());
        let mut unread = [0_u8; 8];
        let count = rustix::io::read(&terminal.input, &mut unread).unwrap();
        assert_eq!(&unread[..count], b"typed");
        assert_eq!(
            read_master(&pty),
            THEME_BACKGROUND_QUERY_WITH_FENCE.as_bytes()
        );
        terminal.disable_raw_mode();
    }

    #[test]
    fn input_around_the_fenced_background_reply_is_kept_as_typeahead() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        rustix::io::write(&pty.master, b"ab\x1b]11;rgb:0000/0000/0000\x07cd\x1b[?1;2c").unwrap();
        assert!(!terminal.query_background().unwrap().light);
        assert_eq!(terminal.take_typeahead(), b"abcd");
        assert_eq!(
            read_master(&pty),
            THEME_BACKGROUND_QUERY_WITH_FENCE.as_bytes()
        );
        terminal.disable_raw_mode();
    }

    #[test]
    fn layout_query_reads_the_window_size() {
        let pty = test_pty::open();
        let size = termios::Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        termios::tcsetwinsize(&pty.master, size).unwrap();
        let terminal = test_pty::terminal(&pty);
        let layout = terminal.query_layout(4).unwrap();
        assert_eq!(layout.rows, 30);
        assert_eq!(layout.cols, 100);
        assert_eq!(layout.content_bottom, 26);
    }

    #[test]
    fn theme_requests_write_their_queries() {
        let pty = test_pty::open();
        let terminal = test_pty::terminal(&pty);
        terminal.enable_theme_notifications().unwrap();
        terminal.request_theme_color_scheme().unwrap();
        terminal.request_theme_response_fence().unwrap();
        terminal.request_theme_background().unwrap();
        assert_eq!(
            read_master(&pty),
            b"\x1b[?2031h\x1b[?996n\x1b[c\x1b]11;?\x1b\\\x1b[c"
        );
    }

    #[test]
    fn the_input_descriptor_polls_ready_when_keys_arrive() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        let ready = |terminal: &Terminal, timeout_ms: u64| {
            let input = terminal.input_fd();
            let mut fds = [PollFd::new(&input, PollFlags::IN)];
            let timeout = Timespec::try_from(Duration::from_millis(timeout_ms)).unwrap();
            rustix::event::poll(&mut fds, Some(&timeout)).unwrap() > 0
        };
        assert!(!ready(&terminal, 0));
        rustix::io::write(&pty.master, b"x").unwrap();
        assert!(ready(&terminal, 1000));
        terminal.disable_raw_mode();
    }

    #[test]
    fn opening_uses_the_process_terminal_with_nonblocking_output() {
        if test_pty::in_child() {
            let mut terminal = Terminal::open().unwrap();
            let flags = rustix::fs::fcntl_getfl(&terminal.output).unwrap();
            assert!(flags.contains(OFlags::NONBLOCK));
            let layout = terminal.query_layout(4).unwrap();
            terminal.enable_raw_mode().unwrap();
            let line = format!("opened {}x{}\r\n", layout.rows, layout.cols);
            terminal.write_all(line.as_bytes()).unwrap();
            terminal.disable_raw_mode();
            return;
        }
        let mut session = test_pty::child_session(
            "terminal::shell_runtime::tests::opening_uses_the_process_terminal_with_nonblocking_output",
            &[("TERM", "xterm-256color")],
        );
        test_pty::wait_output(&session, b"opened 24x80");
        assert!(session.wait_exit(test_pty::WAIT).unwrap().success());
    }

    #[test]
    fn opening_without_a_terminal_fails() {
        if test_pty::in_child() {
            assert!(matches!(Terminal::open(), Err(TerminalError::NotATerminal)));
            return;
        }
        let status = test_pty::child_command(
            "terminal::shell_runtime::tests::opening_without_a_terminal_fails",
            &[],
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
        assert!(status.success());
    }

    fn detected_in_child(test: &str, env: &[(&str, &str)]) -> String {
        let output = test_pty::child_command(test, env)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("detected "))
            .unwrap()
            .to_owned()
    }

    #[test]
    fn capabilities_follow_the_environment() {
        if test_pty::in_child() {
            println!("\ndetected {:?}", Capabilities::detect());
            return;
        }
        let test = "terminal::shell_runtime::tests::capabilities_follow_the_environment";
        let cases: [(&[(&str, &str)], Capabilities); 5] = [
            (
                &[("TERM", "xterm-256color")],
                Capabilities {
                    tmux: false,
                    sync_updates: true,
                    history_reset: HistoryReset::EraseScrollback,
                    color: ColorSupport::Truecolor,
                },
            ),
            (
                &[
                    ("TMUX", "/tmp/tmux-1000/default,1,0"),
                    ("TERM", "dumb"),
                    ("TERM_PROGRAM", "Apple_Terminal"),
                ],
                Capabilities {
                    tmux: true,
                    sync_updates: false,
                    history_reset: HistoryReset::EraseScrollback,
                    color: ColorSupport::Palette256,
                },
            ),
            (
                &[
                    ("TERM", "dumb"),
                    ("OH_FX_SYNC_UPDATES", "on"),
                    ("TERM_PROGRAM", "Apple_Terminal"),
                    ("COLORTERM", "24bit"),
                ],
                Capabilities {
                    tmux: false,
                    sync_updates: true,
                    history_reset: HistoryReset::FullReset,
                    color: ColorSupport::Truecolor,
                },
            ),
            (
                &[("TERM", "xterm"), ("FLASH_SYNC_UPDATES", "off")],
                Capabilities {
                    tmux: false,
                    sync_updates: false,
                    history_reset: HistoryReset::EraseScrollback,
                    color: ColorSupport::Truecolor,
                },
            ),
            (
                &[
                    ("TERM", "xterm"),
                    ("OH_FX_SYNC_UPDATES", "1"),
                    ("FLASH_SYNC_UPDATES", "0"),
                ],
                Capabilities {
                    tmux: false,
                    sync_updates: true,
                    history_reset: HistoryReset::EraseScrollback,
                    color: ColorSupport::Truecolor,
                },
            ),
        ];
        for (env, expected) in cases {
            assert_eq!(
                detected_in_child(test, env),
                format!("{expected:?}"),
                "{env:?}"
            );
        }
    }

    #[test]
    fn theme_detection_reads_the_override_or_falls_back_to_colorfgbg() {
        if test_pty::in_child() {
            let mut terminal = Terminal::open().unwrap();
            terminal.enable_raw_mode().unwrap();
            let detection = terminal.detect_theme(None);
            terminal.disable_raw_mode();
            println!("detected {detection:?}");
            return;
        }
        let test = "terminal::shell_runtime::tests::theme_detection_reads_the_override_or_falls_back_to_colorfgbg";
        for (env, expected, probed) in [
            (
                &[("OH_FX_THEME", "light"), ("COLORFGBG", "15;0")][..],
                "ThemeDetection { light: true, rgb: None, pinned: true }",
                false,
            ),
            (
                &[("COLORFGBG", "0;15")],
                "ThemeDetection { light: true, rgb: None, pinned: false }",
                true,
            ),
        ] {
            let mut session = test_pty::child_session(test, env);
            let output = test_pty::wait_output(&session, expected.as_bytes());
            assert!(session.wait_exit(test_pty::WAIT).unwrap().success());
            let queried = output
                .windows(THEME_BACKGROUND_QUERY_WITH_FENCE.len())
                .any(|window| window == THEME_BACKGROUND_QUERY_WITH_FENCE.as_bytes());
            assert_eq!(queried, probed, "{env:?}");
        }
    }
}
