mod proc_fs;
#[cfg(test)]
mod tests;

use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectionError {
    ProcessNotFound,
    Failed(&'static str),
}

impl InspectionError {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ProcessNotFound => "ProcessNotFound",
            Self::Failed(name) => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    start_ticks: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrackedProcess {
    pid: Pid,
    identity: Identity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessSnapshot {
    identity: Identity,
    parent_pid: Option<Pid>,
    process_group: Option<Pid>,
    zombie: bool,
}

trait Effects {
    fn capture(&self, pid: Pid) -> Result<ProcessSnapshot, InspectionError>;
    fn send(&self, pid: Pid, signal: Signal) -> Result<(), Errno>;
}

struct SystemEffects;

impl Effects for SystemEffects {
    fn capture(&self, pid: Pid) -> Result<ProcessSnapshot, InspectionError> {
        proc_fs::capture_snapshot(pid)
    }

    fn send(&self, pid: Pid, signal: Signal) -> Result<(), Errno> {
        kill_process(pid, signal)
    }
}

#[derive(Debug, Default)]
pub(crate) struct Tracker {
    root: Option<TrackedProcess>,
    processes: Vec<TrackedProcess>,
}

impl Tracker {
    pub(crate) fn track_root(&mut self, root_pid: Pid) -> Result<(), InspectionError> {
        match proc_fs::capture_snapshot(root_pid) {
            Ok(snapshot) => {
                self.root = Some(TrackedProcess {
                    pid: root_pid,
                    identity: snapshot.identity,
                });
                Ok(())
            }
            Err(InspectionError::ProcessNotFound) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn refresh(&mut self) -> Result<(), InspectionError> {
        if let Some(root) = self.root {
            self.append_direct_children(root)?;
        }
        let mut parent_index = 0;
        while let Some(&parent) = self.processes.get(parent_index) {
            parent_index += 1;
            let Ok(actual) = proc_fs::capture_snapshot(parent.pid) else {
                continue;
            };
            if should_traverse_parent(parent.identity, actual.identity) {
                self.append_direct_children(parent)?;
            }
        }
        Ok(())
    }

    pub(crate) fn refresh_additional_root(&mut self, root_pid: Pid) -> Result<(), InspectionError> {
        let snapshot = match proc_fs::capture_snapshot(root_pid) {
            Ok(snapshot) => snapshot,
            Err(InspectionError::ProcessNotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        self.append_direct_children(TrackedProcess {
            pid: root_pid,
            identity: snapshot.identity,
        })
    }

    pub(crate) fn signal_all(&self, signal: Signal) -> usize {
        self.signal_processes_with(signal, None, &SystemEffects)
    }

    pub(crate) fn signal_outside_process_group(
        &self,
        signal: Signal,
        preserved_group: Pid,
    ) -> usize {
        self.signal_processes_with(signal, Some(preserved_group), &SystemEffects)
    }

    pub(crate) fn any_alive(&self) -> bool {
        self.newest_first().any(|process| {
            SystemEffects.capture(process.pid).is_ok_and(|actual| {
                process.identity == actual.identity && snapshot_is_alive(actual)
            })
        })
    }

    fn newest_first(&self) -> impl Iterator<Item = TrackedProcess> + '_ {
        self.processes.iter().rev().chain(&self.root).copied()
    }

    fn signal_processes_with(
        &self,
        signal: Signal,
        preserved_group: Option<Pid>,
        effects: &impl Effects,
    ) -> usize {
        self.newest_first()
            .filter(|&process| signal_tracked_process(process, signal, preserved_group, effects))
            .count()
    }

    fn append_direct_children(&mut self, parent: TrackedProcess) -> Result<(), InspectionError> {
        if !parent_identity_matches(parent)? {
            return Ok(());
        }
        for task in proc_fs::tasks(parent.pid)? {
            let Some(children) = proc_fs::task_children(parent.pid, task)? else {
                continue;
            };
            if !parent_identity_matches(parent)? {
                return Ok(());
            }
            for child in children {
                self.track_child(child, parent.pid)?;
            }
        }
        Ok(())
    }

    fn track_child(&mut self, pid: Pid, expected_parent_pid: Pid) -> Result<(), InspectionError> {
        let snapshot = match proc_fs::capture_snapshot(pid) {
            Ok(snapshot) => snapshot,
            Err(InspectionError::ProcessNotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        if !snapshot_belongs_to_parent(snapshot, expected_parent_pid) {
            return Ok(());
        }
        if self
            .root
            .is_some_and(|root| root.pid == pid && root.identity == snapshot.identity)
        {
            return Ok(());
        }
        match self.processes.iter_mut().find(|process| process.pid == pid) {
            Some(process) => process.identity = snapshot.identity,
            None => self.processes.push(TrackedProcess {
                pid,
                identity: snapshot.identity,
            }),
        }
        Ok(())
    }
}

fn parent_identity_matches(parent: TrackedProcess) -> Result<bool, InspectionError> {
    match proc_fs::capture_snapshot(parent.pid) {
        Ok(snapshot) => Ok(parent.identity == snapshot.identity),
        Err(InspectionError::ProcessNotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

fn signal_tracked_process(
    process: TrackedProcess,
    signal: Signal,
    preserved_group: Option<Pid>,
    effects: &impl Effects,
) -> bool {
    let Ok(actual) = effects.capture(process.pid) else {
        return false;
    };
    if process.identity != actual.identity || actual.zombie {
        return false;
    }
    let Some(process_group) = actual.process_group else {
        return false;
    };
    should_signal_process(Some(process_group), preserved_group)
        && effects.send(process.pid, signal).is_ok()
}

fn should_traverse_parent(expected: Identity, actual: Identity) -> bool {
    expected == actual
}

fn snapshot_belongs_to_parent(snapshot: ProcessSnapshot, expected_parent_pid: Pid) -> bool {
    snapshot.parent_pid == Some(expected_parent_pid)
}

fn should_signal_process(process_group: Option<Pid>, preserved_group: Option<Pid>) -> bool {
    let Some(preserved) = preserved_group else {
        return true;
    };
    process_group.is_some_and(|actual| actual != preserved)
}

fn snapshot_is_alive(snapshot: ProcessSnapshot) -> bool {
    !snapshot.zombie
}
