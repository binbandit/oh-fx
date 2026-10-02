use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

use super::supervision::{CommandTree, Escalation};
use crate::command_runner::TERMINATION_GRACE;
use crate::process_tree::{InspectionError, Tracker};

const CLEANUP_WAIT: Duration = Duration::from_millis(250);
const RESCAN_PAUSE: Duration = Duration::from_millis(1);
const EMPTY_SCANS_TO_SETTLE: u8 = 2;

pub(super) struct TrackedTree {
    tracker: Tracker,
    supervisor: Pid,
}

impl TrackedTree {
    pub(super) fn new(target: Pid, supervisor: Pid) -> Result<Self, &'static str> {
        let mut tracker = Tracker::default();
        tracker.track_root(target).map_err(InspectionError::name)?;
        Ok(Self {
            tracker,
            supervisor,
        })
    }

    fn refresh(&mut self) -> Result<(), &'static str> {
        self.tracker
            .refresh_additional_root(self.supervisor)
            .and_then(|()| self.tracker.refresh())
            .map_err(InspectionError::name)
    }

    fn kill_left_behind(&mut self, forced: bool) -> Result<(), &'static str> {
        self.refresh()?;
        if forced {
            self.tracker.signal_all(Signal::KILL);
        } else {
            self.tracker.signal_attached(Signal::KILL, self.supervisor);
        }
        Ok(())
    }

    fn left_behind_alive(&self, forced: bool) -> bool {
        if forced {
            self.tracker.any_alive()
        } else {
            self.tracker.any_attached_alive(self.supervisor)
        }
    }
}

impl CommandTree for TrackedTree {
    fn stop_gracefully(&mut self) -> Result<(), &'static str> {
        let walked = self.refresh();
        let _ = kill_process_group(self.supervisor, Signal::TERM);
        walked?;
        self.tracker
            .signal_outside_process_group(Signal::TERM, self.supervisor);
        Ok(())
    }

    fn force(&mut self) -> Result<(), &'static str> {
        self.refresh()?;
        self.tracker.signal_all(Signal::KILL);
        Ok(())
    }

    fn settle_termination(
        &mut self,
        started: Instant,
        escalation: &mut dyn Escalation,
    ) -> Result<(), &'static str> {
        let mut empty_scans = 0;
        loop {
            self.refresh()?;
            if self.tracker.is_empty() {
                return Ok(());
            }
            let elapsed = started.elapsed();
            let forced = elapsed >= TERMINATION_GRACE || escalation.forced();
            if forced {
                self.tracker.signal_all(Signal::KILL);
            }
            if self.tracker.any_alive() {
                empty_scans = 0;
            } else {
                empty_scans += 1;
                if empty_scans >= EMPTY_SCANS_TO_SETTLE {
                    return Ok(());
                }
            }
            if forced && elapsed >= TERMINATION_GRACE + CLEANUP_WAIT {
                return Ok(());
            }
            escalation.pause(RESCAN_PAUSE);
        }
    }

    fn settle_completion(&mut self, escalation: &mut dyn Escalation) -> Result<(), &'static str> {
        let started = Instant::now();
        let mut empty_scans = 0;
        while started.elapsed() < CLEANUP_WAIT {
            let forced = escalation.forced();
            self.kill_left_behind(forced)?;
            if self.tracker.is_empty() {
                return Ok(());
            }
            if self.left_behind_alive(forced) {
                empty_scans = 0;
            } else {
                empty_scans += 1;
                if empty_scans >= EMPTY_SCANS_TO_SETTLE {
                    return Ok(());
                }
            }
            escalation.pause(RESCAN_PAUSE);
        }
        self.kill_left_behind(escalation.forced())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use rustix::process::{Pid, Signal, kill_process};

    use super::TrackedTree;
    use crate::command_runner::foreground_session::supervision::{CommandTree, Escalation};

    const SESSION: &str = "import os, sys, time
os.setsid()
ready_r, ready_w = os.pipe()
attached = os.fork()
if attached == 0:
    time.sleep(30)
    os._exit(0)
daemon = os.fork()
if daemon == 0:
    os.setsid()
    os.write(ready_w, b'R')
    time.sleep(30)
    os._exit(0)
os.read(ready_r, 1)
sys.stdout.write('%d %d\\n' % (attached, daemon))
sys.stdout.flush()
time.sleep(30)
";

    struct Escalated(bool);

    impl Escalation for Escalated {
        fn forced(&mut self) -> bool {
            self.0
        }

        fn pause(&mut self, duration: Duration) {
            thread::sleep(duration);
        }
    }

    struct Session {
        leader: Child,
        attached: Pid,
        daemon: Pid,
    }

    impl Session {
        fn start() -> Self {
            let mut leader = Command::new("python3")
                .args(["-c", SESSION])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .expect("start the session leader");
            let mut line = String::new();
            BufReader::new(leader.stdout.take().expect("the leader's stdout"))
                .read_line(&mut line)
                .expect("the leader reports its children");
            let mut pids = line
                .split_whitespace()
                .filter_map(|pid| pid.parse().ok())
                .filter_map(Pid::from_raw);
            let (Some(attached), Some(daemon)) = (pids.next(), pids.next()) else {
                panic!("the leader reported {line:?}");
            };
            Self {
                leader,
                attached,
                daemon,
            }
        }

        fn settle(&mut self, forced: bool) {
            let leader = Pid::from_child(&self.leader);
            let mut tree = TrackedTree::new(leader, leader).expect("track the session");
            tree.settle_completion(&mut Escalated(forced))
                .expect("settle the session");
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if running(self.daemon) {
                let _ = kill_process(self.daemon, Signal::KILL);
            }
            let _ = self.leader.kill();
            let _ = self.leader.wait();
        }
    }

    fn running(pid: Pid) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, fields)| !fields.trim_start().starts_with('Z'))
        })
    }

    #[test]
    fn natural_completion_keeps_a_daemon_unless_the_stop_is_forced() {
        let mut kept = Session::start();
        kept.settle(false);
        assert!(!running(kept.attached), "an attached process survived");
        assert!(running(kept.daemon), "natural completion killed a daemon");

        let mut forced = Session::start();
        forced.settle(true);
        assert!(!running(forced.attached), "an attached process survived");
        assert!(!running(forced.daemon), "a forced stop kept a daemon");
    }
}
