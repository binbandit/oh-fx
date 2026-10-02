use std::time::Instant;

use rustix::process::Pid;

use super::group_tree::GroupTree;
use super::supervision::CommandTree;
use super::tracked_tree::TrackedTree;
use crate::process_tree::proc_shows_own_pid_namespace;

pub(super) enum LinuxTree {
    Tracked(TrackedTree),
    Grouped(GroupTree),
}

impl LinuxTree {
    pub(super) fn new(target: Pid, session: Pid) -> Result<Self, &'static str> {
        if !proc_shows_own_pid_namespace() {
            return Ok(Self::Grouped(GroupTree::new(session)));
        }
        TrackedTree::new(target, session).map(Self::Tracked)
    }

    fn tree(&mut self) -> &mut dyn CommandTree {
        match self {
            Self::Tracked(tree) => tree,
            Self::Grouped(tree) => tree,
        }
    }
}

impl CommandTree for LinuxTree {
    fn stop_gracefully(&mut self) -> Result<(), &'static str> {
        self.tree().stop_gracefully()
    }

    fn force(&mut self) -> Result<(), &'static str> {
        self.tree().force()
    }

    fn settle_termination(&mut self, started: Instant, forced: bool) -> Result<(), &'static str> {
        self.tree().settle_termination(started, forced)
    }

    fn settle_completion(&mut self) -> Result<(), &'static str> {
        self.tree().settle_completion()
    }
}
