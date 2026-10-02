use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, kill_process, wait};
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR2};

use crate::command_runner::TERMINATION_GRACE;

pub(in crate::command_runner) const FORCE_SIGNAL: Signal = Signal::USR1;
const SURVIVED_SIGNALS: [i32; 4] = [SIGINT, SIGHUP, SIGQUIT, SIGUSR2];
const READ_BYTES: usize = 64;
const LONGEST_POLL_MILLISECONDS: u32 = i32::MAX.unsigned_abs();

pub(super) trait CommandTree {
    fn stop_gracefully(&mut self) -> Result<(), &'static str>;
    fn force(&mut self) -> Result<(), &'static str>;
    fn settle_termination(&mut self, started: Instant, forced: bool) -> Result<(), &'static str>;
    fn settle_completion(&mut self) -> Result<(), &'static str>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminationRequest {
    None,
    Graceful,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminationAction {
    None,
    BeginGraceful,
    Force,
}

pub(super) struct Requests {
    graceful: Arc<AtomicBool>,
    force: Arc<AtomicBool>,
    wake: UnixStream,
}

impl Requests {
    pub(super) fn register() -> io::Result<Self> {
        let graceful = Arc::new(AtomicBool::new(false));
        let force = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(SIGTERM, Arc::clone(&graceful))?;
        signal_hook::flag::register(FORCE_SIGNAL.as_raw(), Arc::clone(&force))?;
        for signal in SURVIVED_SIGNALS {
            signal_hook::flag::register(signal, Arc::default())?;
        }
        let (wake, alarm) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        for signal in [SIGTERM, FORCE_SIGNAL.as_raw(), SIGCHLD] {
            signal_hook::low_level::pipe::register(signal, alarm.try_clone()?)?;
        }
        Ok(Self {
            graceful,
            force,
            wake,
        })
    }

    fn observed(&self) -> TerminationRequest {
        if self.force.load(Ordering::SeqCst) {
            TerminationRequest::Force
        } else if self.graceful.load(Ordering::SeqCst) {
            TerminationRequest::Graceful
        } else {
            TerminationRequest::None
        }
    }

    fn drain(&self) {
        let mut buffer = [0; READ_BYTES];
        while matches!((&self.wake).read(&mut buffer), Ok(length) if length > 0) {}
    }
}

pub(super) struct Supervision<T> {
    target: Pid,
    deadline: Option<Instant>,
    requests: Requests,
    tree: T,
    status: Option<ExitStatus>,
    owner_alive: bool,
    termination_started: Option<Instant>,
    forced: bool,
}

impl<T: CommandTree> Supervision<T> {
    pub(super) fn new(target: Pid, deadline: Option<Instant>, requests: Requests, tree: T) -> Self {
        Self {
            target,
            deadline,
            requests,
            tree,
            status: None,
            owner_alive: true,
            termination_started: None,
            forced: false,
        }
    }

    pub(super) fn wait_for_target(&mut self) -> Result<ExitStatus, &'static str> {
        loop {
            self.requests.drain();
            self.reap();
            let now = Instant::now();
            let request = request_at_deadline(
                self.requests.observed(),
                self.owner_alive,
                self.deadline,
                now,
            );
            if request == TerminationRequest::Force {
                self.requests.force.store(true, Ordering::SeqCst);
            }
            self.advance(request, now)?;
            if let Some(status) = self.status {
                return Ok(status);
            }
            self.wait_for_event();
        }
    }

    pub(super) fn settle(&mut self) -> Result<(), &'static str> {
        match self.termination_started {
            Some(started) => self.tree.settle_termination(started, self.forced),
            None => self.tree.settle_completion(),
        }
    }

    pub(super) fn kill_target(&self) {
        if self.status.is_none() {
            let _ = kill_process(self.target, Signal::KILL);
        }
    }

    fn reap(&mut self) {
        while let Ok(Some((pid, status))) = wait(WaitOptions::NOHANG) {
            if pid == self.target && self.status.is_none() {
                self.status = Some(ExitStatus::from_raw(status.as_raw()));
            }
        }
    }

    fn advance(&mut self, request: TerminationRequest, now: Instant) -> Result<(), &'static str> {
        match decide_termination_action(request, self.termination_started, self.forced, now) {
            TerminationAction::None => Ok(()),
            TerminationAction::BeginGraceful => {
                self.termination_started = Some(now);
                self.tree.stop_gracefully()
            }
            TerminationAction::Force => {
                self.termination_started.get_or_insert(now);
                self.forced = true;
                self.kill_target();
                self.tree.force()
            }
        }
    }

    fn wait_for_event(&mut self) {
        let timeout = poll_timeout(self.next_timer(), Instant::now());
        let stdin = io::stdin();
        let watched = if self.owner_alive { 2 } else { 1 };
        let mut descriptors = [
            PollFd::new(&self.requests.wake, PollFlags::IN),
            PollFd::new(&stdin, PollFlags::IN),
        ];
        let polled = poll(&mut descriptors[..watched], timeout.as_ref());
        let owner_ready = polled.is_ok() && watched == 2 && !descriptors[1].revents().is_empty();
        if owner_ready {
            self.owner_alive = owner_still_open(&stdin);
        }
    }

    fn next_timer(&self) -> Option<Instant> {
        if self.forced {
            return None;
        }
        let grace_ends = self
            .termination_started
            .map(|started| started + TERMINATION_GRACE);
        self.deadline.into_iter().chain(grace_ends).min()
    }
}

fn poll_timeout(next_timer: Option<Instant>, now: Instant) -> Option<Timespec> {
    let wait = next_timer?.saturating_duration_since(now);
    Timespec::try_from(wait.min(Duration::from_millis(LONGEST_POLL_MILLISECONDS.into()))).ok()
}

fn owner_still_open(stdin: &io::Stdin) -> bool {
    let mut buffer = [0; READ_BYTES];
    match rustix::io::read(stdin, &mut buffer) {
        Ok(length) => length > 0,
        Err(error) => matches!(error, Errno::INTR | Errno::AGAIN),
    }
}

fn decide_termination_action(
    request: TerminationRequest,
    termination_started: Option<Instant>,
    forced: bool,
    now: Instant,
) -> TerminationAction {
    if forced || request == TerminationRequest::None {
        return TerminationAction::None;
    }
    if request == TerminationRequest::Force {
        return TerminationAction::Force;
    }
    let Some(started) = termination_started else {
        return TerminationAction::BeginGraceful;
    };
    if now.saturating_duration_since(started) >= TERMINATION_GRACE {
        return TerminationAction::Force;
    }
    TerminationAction::None
}

fn request_at_deadline(
    observed: TerminationRequest,
    owner_alive: bool,
    deadline: Option<Instant>,
    now: Instant,
) -> TerminationRequest {
    if !owner_alive {
        return TerminationRequest::Force;
    }
    if observed != TerminationRequest::None {
        return observed;
    }
    match deadline {
        Some(deadline) if now >= deadline => TerminationRequest::Force,
        _ => TerminationRequest::None,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use rustix::process::{Pid, Signal};

    use super::{
        CommandTree, Requests, Supervision, TerminationAction, TerminationRequest,
        decide_termination_action, poll_timeout, request_at_deadline,
    };

    const EXIT_LIMIT: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(10);

    struct UnreachableTree;

    impl CommandTree for UnreachableTree {
        fn stop_gracefully(&mut self) -> Result<(), &'static str> {
            Ok(())
        }

        fn force(&mut self) -> Result<(), &'static str> {
            Ok(())
        }

        fn settle_termination(&mut self, _: Instant, _: bool) -> Result<(), &'static str> {
            Ok(())
        }

        fn settle_completion(&mut self) -> Result<(), &'static str> {
            Ok(())
        }
    }

    #[test]
    fn a_forced_stop_kills_a_command_its_tree_cannot_reach() {
        let mut command = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("the command starts");
        let (wake, _alarm) = UnixStream::pair().expect("the wake pipe opens");
        let requests = Requests {
            graceful: Arc::default(),
            force: Arc::default(),
            wake,
        };
        let mut supervision =
            Supervision::new(Pid::from_child(&command), None, requests, UnreachableTree);
        let forced = supervision.advance(TerminationRequest::Force, Instant::now());
        let begun = Instant::now();
        let status = loop {
            match command.try_wait().expect("the command can be waited for") {
                Some(status) => break Some(status),
                None if begun.elapsed() >= EXIT_LIMIT => break None,
                None => thread::sleep(POLL),
            }
        };
        if status.is_none() {
            let _ = command.kill();
            let _ = command.wait();
        }
        assert_eq!(forced, Ok(()));
        assert_eq!(
            status.and_then(|status| status.signal()),
            Some(Signal::KILL.as_raw()),
            "the forced stop left the command running"
        );
        assert_eq!(supervision.next_timer(), None);
    }

    #[test]
    fn a_distant_timer_waits_no_longer_than_poll_can_express() {
        let now = Instant::now();
        let soon = poll_timeout(Some(now + Duration::from_millis(5)), now)
            .expect("a near timer sets a timeout");
        assert_eq!((soon.tv_sec, soon.tv_nsec), (0, 5_000_000));
        assert_eq!(poll_timeout(None, now), None);
        assert_eq!(
            poll_timeout(Some(now), now + Duration::from_millis(1)).map(|due| due.tv_nsec),
            Some(0)
        );
        let distant = poll_timeout(Some(now + Duration::from_hours(720)), now)
            .expect("a distant timer sets a timeout");
        let milliseconds = distant.tv_sec * 1000 + (distant.tv_nsec + 999_999) / 1_000_000;
        assert!(
            milliseconds <= i64::from(i32::MAX),
            "{milliseconds} ms does not fit poll's timeout"
        );
    }

    #[test]
    fn foreground_force_request_dominates_graceful_termination() {
        let base = Instant::now();
        let at = |milliseconds| base + Duration::from_millis(milliseconds);
        assert_eq!(
            decide_termination_action(TerminationRequest::None, None, false, at(1000)),
            TerminationAction::None
        );
        assert_eq!(
            decide_termination_action(TerminationRequest::Graceful, None, false, at(1000)),
            TerminationAction::BeginGraceful
        );
        assert_eq!(
            decide_termination_action(
                TerminationRequest::Graceful,
                Some(at(1000)),
                false,
                at(1699)
            ),
            TerminationAction::None
        );
        assert_eq!(
            decide_termination_action(
                TerminationRequest::Graceful,
                Some(at(1000)),
                false,
                at(1700)
            ),
            TerminationAction::Force
        );
        assert_eq!(
            decide_termination_action(TerminationRequest::Force, Some(at(1000)), false, at(1001)),
            TerminationAction::Force
        );
        assert_eq!(
            decide_termination_action(TerminationRequest::Force, Some(at(1000)), true, at(2000)),
            TerminationAction::None
        );

        assert_eq!(
            request_at_deadline(TerminationRequest::None, true, Some(at(1700)), at(1699)),
            TerminationRequest::None
        );
        assert_eq!(
            request_at_deadline(TerminationRequest::None, true, Some(at(1700)), at(1700)),
            TerminationRequest::Force
        );
        assert_eq!(
            request_at_deadline(TerminationRequest::Graceful, true, Some(at(1700)), at(2000)),
            TerminationRequest::Graceful
        );
        assert_eq!(
            request_at_deadline(TerminationRequest::None, false, None, at(1000)),
            TerminationRequest::Force
        );
        assert_eq!(
            request_at_deadline(
                TerminationRequest::Graceful,
                false,
                Some(at(1700)),
                at(1000)
            ),
            TerminationRequest::Force
        );
    }
}
