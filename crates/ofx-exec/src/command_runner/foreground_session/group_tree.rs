use std::time::Instant;

use rustix::process::{Pid, Signal, kill_process_group};

use super::supervision::{CommandTree, Escalation};

pub(super) struct GroupTree {
    group: Pid,
}

impl GroupTree {
    pub(super) fn new(group: Pid) -> Self {
        Self { group }
    }
}

impl CommandTree for GroupTree {
    fn stop_gracefully(&mut self) -> Result<(), &'static str> {
        let _ = kill_process_group(self.group, Signal::TERM);
        Ok(())
    }

    fn force(&mut self) -> Result<(), &'static str> {
        let _ = kill_process_group(self.group, Signal::KILL);
        Ok(())
    }

    fn settle_termination(
        &mut self,
        _: Instant,
        _: &mut dyn Escalation,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn settle_completion(&mut self, _: &mut dyn Escalation) -> Result<(), &'static str> {
        Ok(())
    }
}
