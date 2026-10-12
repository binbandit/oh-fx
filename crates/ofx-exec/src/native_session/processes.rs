use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};

#[cfg(target_os = "linux")]
use crate::process_tree::{Tracker, instance_token, proc_shows_own_pid_namespace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellIdentity(Option<String>);

impl ShellIdentity {
    pub(super) fn matches(&self, shell: Pid) -> bool {
        self.0
            .as_ref()
            .is_none_or(|expected| Kernel::current(shell).as_ref() == Some(expected))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupDelivery {
    Delivered,
    Missing,
    Failed,
}

pub(super) trait Identities {
    fn identify(shell: Pid) -> Option<ShellIdentity>;
    fn current(shell: Pid) -> Option<String>;
}

pub(super) trait Descendants: Sized {
    fn record(shell: Pid) -> Option<Self>;
    fn signal_outside_group(&self, signal: Signal, shell: Pid) -> bool;
    fn signal_every(&self, signal: Signal);
    fn any_alive(&self) -> bool;
}

pub(super) struct Kernel;

#[cfg(target_os = "linux")]
pub(super) type Tree = LinuxTree;

#[cfg(not(target_os = "linux"))]
pub(super) type Tree = GroupOnly;

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(super) struct LinuxTree(Tracker);

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub(super) struct GroupOnly;

#[cfg(target_os = "linux")]
impl Identities for Kernel {
    fn identify(shell: Pid) -> Option<ShellIdentity> {
        if !proc_shows_own_pid_namespace() {
            return Some(ShellIdentity(None));
        }
        instance_token(shell)
            .ok()
            .map(|token| ShellIdentity(Some(token)))
    }

    fn current(shell: Pid) -> Option<String> {
        instance_token(shell).ok()
    }
}

#[cfg(not(target_os = "linux"))]
impl Identities for Kernel {
    fn identify(_: Pid) -> Option<ShellIdentity> {
        Some(ShellIdentity(None))
    }

    fn current(_: Pid) -> Option<String> {
        None
    }
}

#[cfg(target_os = "linux")]
impl Descendants for LinuxTree {
    fn record(shell: Pid) -> Option<Self> {
        let mut tracker = Tracker::default();
        if proc_shows_own_pid_namespace() {
            tracker.track_root(shell).ok()?;
            tracker.refresh().ok()?;
        }
        Some(Self(tracker))
    }

    fn signal_outside_group(&self, signal: Signal, shell: Pid) -> bool {
        self.0
            .signal_outside_process_group(signal, shell)
            .incomplete
    }

    fn signal_every(&self, signal: Signal) {
        self.0.signal_every(signal);
    }

    fn any_alive(&self) -> bool {
        self.0.any_alive()
    }
}

#[cfg(not(target_os = "linux"))]
impl Descendants for GroupOnly {
    fn record(_: Pid) -> Option<Self> {
        Some(Self)
    }

    fn signal_outside_group(&self, _: Signal, _: Pid) -> bool {
        false
    }

    fn signal_every(&self, _: Signal) {}

    fn any_alive(&self) -> bool {
        false
    }
}

pub(super) fn signal_group(shell: Pid, signal: Signal) -> GroupDelivery {
    loop {
        match kill_process_group(shell, signal) {
            Ok(()) => return GroupDelivery::Delivered,
            Err(Errno::INTR) => {}
            Err(Errno::SRCH) => return GroupDelivery::Missing,
            Err(_) => return GroupDelivery::Failed,
        }
    }
}

pub(super) fn group_missing(shell: Pid) -> bool {
    loop {
        match test_kill_process_group(shell) {
            Err(Errno::INTR) => {}
            Err(Errno::SRCH) => return true,
            _ => return false,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::process::Command;

    use rustix::process::{getpid, kill_process};

    use super::*;

    #[test]
    fn a_shell_identity_names_its_process_until_the_process_is_gone() {
        let own = Kernel::identify(getpid()).unwrap();
        if let Some(token) = &own.0 {
            let fields: Vec<&str> = token.split(':').collect();
            assert_eq!(fields.len(), 3, "{token}");
            assert_eq!(fields[0], "linux");
            assert_eq!(fields[1].len(), 32);
            assert!(fields[2].parse::<u64>().is_ok(), "{token}");
        }
        assert!(own.matches(getpid()));
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = Pid::from_child(&child);
        let identity = Kernel::identify(pid).unwrap();
        assert!(identity.matches(pid));
        if identity.0.is_some() {
            assert!(!identity.matches(getpid()));
            assert!(
                !ShellIdentity(Some("linux:00000000000000000000000000000000:1".into()))
                    .matches(pid)
            );
        }
        kill_process(pid, Signal::KILL).unwrap();
        child.wait().unwrap();
        assert_eq!(identity.matches(pid), identity.0.is_none());
    }
}
