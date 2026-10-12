use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, kill_process_group, test_kill_process, test_kill_process_group,
};

#[cfg(target_os = "linux")]
use crate::process_tree::{
    Tracker, group_session, instance, instance_token, proc_shows_own_pid_namespace,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellIdentity(Option<Recorded>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Recorded {
    token: String,
    session: Option<Pid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellCheck {
    Same,
    Gone,
    Replaced,
}

impl ShellIdentity {
    pub(super) fn check(&self, shell: Pid) -> ShellCheck {
        let Some(expected) = &self.0 else {
            return ShellCheck::Same;
        };
        match Kernel::current(shell) {
            Some(current) if current == expected.token => ShellCheck::Same,
            None if process_missing(shell) => ShellCheck::Gone,
            _ => ShellCheck::Replaced,
        }
    }

    pub(super) fn holds_group(&self, group: Pid) -> bool {
        self.0.as_ref().is_some_and(|recorded| {
            recorded.session.is_some() && Kernel::group_session(group) == recorded.session
        })
    }

    #[cfg(test)]
    pub(super) fn recorded(token: &str, session: Pid) -> Self {
        Self(Some(Recorded {
            token: token.to_owned(),
            session: Some(session),
        }))
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
    fn group_session(group: Pid) -> Option<Pid>;
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
        instance(shell).ok().map(|instance| {
            ShellIdentity(Some(Recorded {
                token: instance.token,
                session: instance.session,
            }))
        })
    }

    fn current(shell: Pid) -> Option<String> {
        instance_token(shell).ok()
    }

    fn group_session(group: Pid) -> Option<Pid> {
        group_session(group)
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

    fn group_session(_: Pid) -> Option<Pid> {
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

fn process_missing(shell: Pid) -> bool {
    loop {
        match test_kill_process(shell) {
            Err(Errno::INTR) => {}
            Err(Errno::SRCH) => return true,
            _ => return false,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use rustix::process::{getpid, getsid, kill_process};

    use super::*;

    #[test]
    fn a_shell_identity_names_its_process_until_the_process_is_gone() {
        let own = Kernel::identify(getpid()).unwrap();
        if let Some(recorded) = &own.0 {
            assert_eq!(recorded.session, Some(getsid(None).unwrap()));
            let token = &recorded.token;
            let fields: Vec<&str> = token.split(':').collect();
            assert_eq!(fields.len(), 3, "{token}");
            assert_eq!(fields[0], "linux");
            assert_eq!(fields[1].len(), 32);
            assert!(fields[2].parse::<u64>().is_ok(), "{token}");
        }
        assert_eq!(own.check(getpid()), ShellCheck::Same);
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = Pid::from_child(&child);
        let identity = Kernel::identify(pid).unwrap();
        assert_eq!(identity.check(pid), ShellCheck::Same);
        if identity.0.is_some() {
            assert_eq!(identity.check(getpid()), ShellCheck::Replaced);
            assert_eq!(
                ShellIdentity::recorded(
                    "linux:00000000000000000000000000000000:1",
                    getsid(None).unwrap()
                )
                .check(pid),
                ShellCheck::Replaced
            );
        }
        kill_process(pid, Signal::KILL).unwrap();
        child.wait().unwrap();
        let gone = if identity.0.is_some() {
            ShellCheck::Gone
        } else {
            ShellCheck::Same
        };
        assert_eq!(identity.check(pid), gone);
    }

    #[test]
    fn a_group_belongs_to_a_shell_only_through_its_recorded_session() {
        let session = getsid(None).unwrap();
        let mut leader = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Pid::from_child(&leader);
        let token = "linux:00000000000000000000000000000000:1";
        let other = Pid::from_raw(session.as_raw_nonzero().get() + 1).unwrap();
        let ours = ShellIdentity::recorded(token, session).holds_group(group);
        let theirs = ShellIdentity::recorded(token, other).holds_group(group);
        kill_process(group, Signal::KILL).unwrap();
        leader.wait().unwrap();
        let after = ShellIdentity::recorded(token, session).holds_group(group);
        if proc_shows_own_pid_namespace() {
            assert!(ours);
        }
        assert!(!theirs);
        assert!(!after);
        assert!(!ShellIdentity(None).holds_group(group));
    }
}
