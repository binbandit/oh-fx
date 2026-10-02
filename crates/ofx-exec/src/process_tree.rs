mod proc_fs;
#[cfg(test)]
mod tests;

use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectionError {
    ProcessNotFound,
    Denied(&'static str),
    Failed(&'static str),
}

impl InspectionError {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ProcessNotFound => "ProcessNotFound",
            Self::Denied(name) | Self::Failed(name) => name,
        }
    }
}

pub(crate) fn proc_shows_own_pid_namespace() -> bool {
    proc_fs::shows_own_pid_namespace()
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
    session: Option<Pid>,
    zombie: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionStanding {
    Attached,
    Detached,
    Gone,
}

fn completion_standing(command_session: Pid, session: Option<Pid>) -> CompletionStanding {
    match session {
        Some(value) if value == command_session => CompletionStanding::Attached,
        Some(_) => CompletionStanding::Detached,
        None => CompletionStanding::Attached,
    }
}

trait Effects {
    fn capture(&self, pid: Pid) -> Result<ProcessSnapshot, InspectionError>;
    fn tasks(&self, pid: Pid) -> Result<Vec<Pid>, InspectionError>;
    fn task_children(&self, pid: Pid, task: Pid) -> Result<Option<Vec<Pid>>, InspectionError>;
    fn send(&self, pid: Pid, signal: Signal) -> Result<(), Errno>;
}

struct SystemEffects;

impl Effects for SystemEffects {
    fn capture(&self, pid: Pid) -> Result<ProcessSnapshot, InspectionError> {
        proc_fs::capture_snapshot(pid)
    }

    fn tasks(&self, pid: Pid) -> Result<Vec<Pid>, InspectionError> {
        proc_fs::tasks(pid)
    }

    fn task_children(&self, pid: Pid, task: Pid) -> Result<Option<Vec<Pid>>, InspectionError> {
        proc_fs::task_children(pid, task)
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
        self.track_root_with(root_pid, &SystemEffects)
    }

    pub(crate) fn refresh(&mut self) -> Result<(), InspectionError> {
        self.refresh_with(&SystemEffects)
    }

    pub(crate) fn refresh_additional_root(&mut self, root_pid: Pid) -> Result<(), InspectionError> {
        self.refresh_additional_root_with(root_pid, &SystemEffects)
    }

    fn track_root_with(
        &mut self,
        root_pid: Pid,
        effects: &impl Effects,
    ) -> Result<(), InspectionError> {
        if let Some(snapshot) = visible(effects.capture(root_pid))? {
            self.root = Some(TrackedProcess {
                pid: root_pid,
                identity: snapshot.identity,
            });
        }
        Ok(())
    }

    fn refresh_with(&mut self, effects: &impl Effects) -> Result<(), InspectionError> {
        if let Some(root) = self.root {
            self.append_direct_children(root, effects)?;
        }
        let mut parent_index = 0;
        while let Some(&parent) = self.processes.get(parent_index) {
            parent_index += 1;
            let Ok(actual) = effects.capture(parent.pid) else {
                continue;
            };
            if should_traverse_parent(parent.identity, actual.identity) {
                self.append_direct_children(parent, effects)?;
            }
        }
        Ok(())
    }

    fn refresh_additional_root_with(
        &mut self,
        root_pid: Pid,
        effects: &impl Effects,
    ) -> Result<(), InspectionError> {
        let Some(snapshot) = visible(effects.capture(root_pid))? else {
            return Ok(());
        };
        self.append_direct_children(
            TrackedProcess {
                pid: root_pid,
                identity: snapshot.identity,
            },
            effects,
        )
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

    pub(crate) fn signal_attached(&self, signal: Signal, command_session: Pid) -> usize {
        self.signal_attached_with(signal, command_session, &SystemEffects)
    }

    pub(crate) fn any_attached_alive(&self, command_session: Pid) -> bool {
        self.any_attached_alive_with(command_session, &SystemEffects)
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

    fn signal_attached_with(
        &self,
        signal: Signal,
        command_session: Pid,
        effects: &impl Effects,
    ) -> usize {
        self.newest_first()
            .filter(|&process| {
                completion_standing_with(process, command_session, effects)
                    == CompletionStanding::Attached
                    && signal_tracked_process(process, signal, None, effects)
            })
            .count()
    }

    fn any_attached_alive_with(&self, command_session: Pid, effects: &impl Effects) -> bool {
        self.newest_first().any(|process| {
            completion_standing_with(process, command_session, effects)
                == CompletionStanding::Attached
        })
    }

    fn append_direct_children(
        &mut self,
        parent: TrackedProcess,
        effects: &impl Effects,
    ) -> Result<(), InspectionError> {
        if !parent_identity_matches(parent, effects)? {
            return Ok(());
        }
        let Some(tasks) = visible(effects.tasks(parent.pid))? else {
            return Ok(());
        };
        for task in tasks {
            let Some(Some(children)) = visible(effects.task_children(parent.pid, task))? else {
                continue;
            };
            if !parent_identity_matches(parent, effects)? {
                return Ok(());
            }
            for child in children {
                self.track_child(child, parent.pid, effects)?;
            }
        }
        Ok(())
    }

    fn track_child(
        &mut self,
        pid: Pid,
        expected_parent_pid: Pid,
        effects: &impl Effects,
    ) -> Result<(), InspectionError> {
        let Some(snapshot) = visible(effects.capture(pid))? else {
            return Ok(());
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

fn parent_identity_matches(
    parent: TrackedProcess,
    effects: &impl Effects,
) -> Result<bool, InspectionError> {
    Ok(visible(effects.capture(parent.pid))?
        .is_some_and(|snapshot| parent.identity == snapshot.identity))
}

fn visible<T>(inspected: Result<T, InspectionError>) -> Result<Option<T>, InspectionError> {
    match inspected {
        Ok(value) => Ok(Some(value)),
        Err(InspectionError::ProcessNotFound | InspectionError::Denied(_)) => Ok(None),
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

fn completion_standing_with(
    process: TrackedProcess,
    command_session: Pid,
    effects: &impl Effects,
) -> CompletionStanding {
    let actual = match effects.capture(process.pid) {
        Ok(actual) => actual,
        Err(InspectionError::ProcessNotFound) => return CompletionStanding::Gone,
        Err(InspectionError::Denied(_) | InspectionError::Failed(_)) => {
            return CompletionStanding::Attached;
        }
    };
    if process.identity != actual.identity || !snapshot_is_alive(actual) {
        return CompletionStanding::Gone;
    }
    completion_standing(command_session, actual.session)
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
