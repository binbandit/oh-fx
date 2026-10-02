use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

use super::supervision::CommandTree;
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
        mut forced: bool,
    ) -> Result<(), &'static str> {
        let mut empty_scans = 0;
        loop {
            self.refresh()?;
            let elapsed = started.elapsed();
            forced |= elapsed >= TERMINATION_GRACE;
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
            thread::sleep(RESCAN_PAUSE);
        }
    }

    fn settle_completion(&mut self) -> Result<(), &'static str> {
        Ok(())
    }
}
