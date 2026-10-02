use std::cell::RefCell;
use std::io::Read;
use std::process::{Child, Command, Stdio};

use rustix::io::Errno;
use rustix::process::{Pid, Signal, getpid};

use super::{
    CompletionStanding, Effects, Identity, InspectionError, ProcessSnapshot, TrackedProcess,
    Tracker, completion_standing, proc_fs, should_signal_process, should_traverse_parent,
    snapshot_belongs_to_parent, snapshot_is_alive,
};

type Capture = fn(i32) -> Result<ProcessSnapshot, InspectionError>;
type Children = Result<Option<Vec<Pid>>, InspectionError>;

struct FakeEffects {
    capture: Capture,
    tasks: fn(i32) -> Result<Vec<Pid>, InspectionError>,
    task_children: fn(i32, i32) -> Children,
    send: fn(i32) -> Result<(), Errno>,
    sent: RefCell<Vec<i32>>,
}

impl FakeEffects {
    fn new(capture: Capture) -> Self {
        Self {
            capture,
            tasks: |raw| Ok(vec![pid(raw)]),
            task_children: |_, _| Ok(None),
            send: |_| Ok(()),
            sent: RefCell::default(),
        }
    }

    fn sent(&self) -> Vec<i32> {
        self.sent.borrow().clone()
    }
}

impl Effects for FakeEffects {
    fn capture(&self, pid: Pid) -> Result<ProcessSnapshot, InspectionError> {
        (self.capture)(pid.as_raw_pid())
    }

    fn tasks(&self, pid: Pid) -> Result<Vec<Pid>, InspectionError> {
        (self.tasks)(pid.as_raw_pid())
    }

    fn task_children(&self, pid: Pid, task: Pid) -> Children {
        (self.task_children)(pid.as_raw_pid(), task.as_raw_pid())
    }

    fn send(&self, pid: Pid, _: Signal) -> Result<(), Errno> {
        self.sent.borrow_mut().push(pid.as_raw_pid());
        (self.send)(pid.as_raw_pid())
    }
}

fn pid(raw: i32) -> Pid {
    Pid::from_raw(raw).expect("a positive pid")
}

fn identity(start_ticks: u64) -> Identity {
    Identity { start_ticks }
}

fn snapshot(start_ticks: u64) -> ProcessSnapshot {
    ProcessSnapshot {
        identity: identity(start_ticks),
        parent_pid: Some(pid(1)),
        process_group: Some(pid(1)),
        session: Some(pid(1)),
        zombie: false,
    }
}

fn own_snapshot(raw: i32) -> ProcessSnapshot {
    ProcessSnapshot {
        session: Some(pid(raw)),
        ..grouped(raw, Some(raw))
    }
}

fn in_session(raw: i32, session: Option<i32>) -> ProcessSnapshot {
    ProcessSnapshot {
        session: session.map(pid),
        ..own_snapshot(raw)
    }
}

fn grouped(raw: i32, process_group: Option<i32>) -> ProcessSnapshot {
    ProcessSnapshot {
        process_group: process_group.map(pid),
        ..snapshot(raw.unsigned_abs().into())
    }
}

fn child_of(raw: i32, parent: i32) -> ProcessSnapshot {
    ProcessSnapshot {
        parent_pid: Some(pid(parent)),
        ..own_snapshot(raw)
    }
}

fn tracked(tracker: &Tracker) -> Vec<i32> {
    tracker
        .processes
        .iter()
        .map(|process| process.pid.as_raw_pid())
        .collect()
}

fn walk(children: fn(i32, i32) -> Children, capture: Capture) -> FakeEffects {
    FakeEffects {
        tasks: |raw| match raw {
            11 => Ok(vec![pid(11), pid(21)]),
            13 => Err(InspectionError::Denied("PermissionDenied")),
            _ => Ok(vec![pid(raw)]),
        },
        task_children: children,
        ..FakeEffects::new(capture)
    }
}

fn denied_children(raw: i32, task: i32) -> Children {
    match (raw, task) {
        (10, 10) => Ok(Some(vec![pid(11), pid(12), pid(13)])),
        (11, 11) => Err(InspectionError::Denied("AccessDenied")),
        (11, 21) => Ok(Some(vec![pid(14)])),
        (13, 13) => Ok(Some(vec![pid(15)])),
        _ => Ok(None),
    }
}

fn tracking(pids: impl IntoIterator<Item = i32>) -> Tracker {
    Tracker {
        root: None,
        processes: pids
            .into_iter()
            .map(|raw| TrackedProcess {
                pid: pid(raw),
                identity: identity(raw.unsigned_abs().into()),
            })
            .collect(),
    }
}

#[test]
fn process_group_exclusion_preserves_the_captured_command_grace() {
    assert!(should_signal_process(Some(pid(41)), None));
    assert!(!should_signal_process(Some(pid(41)), Some(pid(41))));
    assert!(should_signal_process(Some(pid(42)), Some(pid(41))));
    assert!(!should_signal_process(None, Some(pid(41))));
}

#[test]
fn stale_process_identities_cannot_become_traversal_roots() {
    assert!(should_traverse_parent(identity(41), identity(41)));
    assert!(!should_traverse_parent(identity(41), identity(42)));
}

#[test]
fn child_admission_binds_the_observed_process_to_its_expected_parent() {
    let observed = ProcessSnapshot {
        identity: identity(42),
        parent_pid: Some(pid(17)),
        process_group: Some(pid(17)),
        session: Some(pid(17)),
        zombie: false,
    };
    assert!(snapshot_belongs_to_parent(observed, pid(17)));
    assert!(!snapshot_belongs_to_parent(observed, pid(18)));
    let orphaned = ProcessSnapshot {
        parent_pid: None,
        ..observed
    };
    assert!(!snapshot_belongs_to_parent(orphaned, pid(17)));
}

#[test]
fn checked_signal_delivery_distinguishes_vanished_stale_and_failed_targets() {
    let effects = FakeEffects {
        send: |raw| match raw {
            17 => Err(Errno::PERM),
            18 => Err(Errno::SRCH),
            _ => Ok(()),
        },
        ..FakeEffects::new(|raw| match raw {
            11 => Err(InspectionError::ProcessNotFound),
            12 => Err(InspectionError::Failed("ProcessIdentityUnavailable")),
            13 => Ok(snapshot(113)),
            14 => Ok(ProcessSnapshot {
                zombie: true,
                ..own_snapshot(14)
            }),
            15 => Ok(grouped(15, None)),
            16 => Ok(grouped(16, Some(41))),
            _ => Ok(grouped(raw, Some(raw + 100))),
        })
    };
    let tracker = tracking(10..19);
    let delivered = tracker.signal_processes_with(Signal::TERM, Some(pid(41)), &effects);
    assert_eq!(delivered, 1);
    assert_eq!(effects.sent(), [18, 17, 10]);
}

#[test]
fn checked_signal_delivery_keeps_vanished_stale_and_excluded_targets_complete() {
    let effects = FakeEffects::new(|raw| match raw {
        21 => Err(InspectionError::ProcessNotFound),
        22 => Ok(snapshot(122)),
        23 => Ok(grouped(23, None)),
        _ => Ok(grouped(raw, Some(41))),
    });
    let tracker = tracking(21..25);
    let delivered = tracker.signal_processes_with(Signal::TERM, Some(pid(41)), &effects);
    assert_eq!(delivered, 0);
    assert!(effects.sent().is_empty());
}

#[test]
fn a_walk_skips_what_it_may_not_inspect_and_keeps_going() {
    let effects = walk(denied_children, |raw| match raw {
        12 => Err(InspectionError::Denied("PermissionDenied")),
        11 | 13 => Ok(child_of(raw, 10)),
        14 => Ok(child_of(14, 11)),
        15 => Ok(child_of(15, 13)),
        _ => Ok(own_snapshot(raw)),
    });
    let mut tracker = Tracker::default();
    tracker
        .track_root_with(pid(10), &effects)
        .expect("the command is readable");
    tracker
        .refresh_with(&effects)
        .expect("a hidden process does not stop the walk");
    assert_eq!(tracked(&tracker), [11, 13, 14]);

    let mut hidden_root = Tracker::default();
    hidden_root
        .track_root_with(pid(12), &effects)
        .expect("a hidden command is not an error");
    assert_eq!(hidden_root.root, None);
}

#[test]
fn a_walk_still_fails_on_errors_other_than_denied_access() {
    let effects = walk(denied_children, |raw| match raw {
        12 => Err(InspectionError::Failed("ProcessIdentityUnavailable")),
        11 | 13 => Ok(child_of(raw, 10)),
        _ => Ok(own_snapshot(raw)),
    });
    let mut tracker = Tracker::default();
    tracker
        .track_root_with(pid(10), &effects)
        .expect("the command is readable");
    assert_eq!(
        tracker.refresh_with(&effects),
        Err(InspectionError::Failed("ProcessIdentityUnavailable"))
    );
}

#[test]
fn natural_completion_detaches_only_processes_that_left_the_command_session() {
    let command_session = pid(500);
    assert_eq!(
        completion_standing(command_session, Some(command_session)),
        CompletionStanding::Attached
    );
    assert_eq!(
        completion_standing(command_session, Some(pid(700))),
        CompletionStanding::Detached
    );
    assert_eq!(
        completion_standing(command_session, None),
        CompletionStanding::Attached
    );
}

#[test]
fn natural_completion_stops_attached_processes_and_keeps_detached_daemons() {
    let effects = FakeEffects::new(|raw| match raw {
        30 | 32 => Ok(in_session(raw, Some(500))),
        34 => Err(InspectionError::ProcessNotFound),
        35 => Ok(snapshot(999)),
        36 => Ok(ProcessSnapshot {
            zombie: true,
            ..in_session(36, Some(500))
        }),
        37 => Ok(in_session(37, None)),
        _ => Ok(own_snapshot(raw)),
    });
    let tracker = tracking(30..38);
    let delivered = tracker.signal_attached_with(Signal::KILL, pid(500), &effects);
    assert_eq!(effects.sent(), [37, 32, 30]);
    assert_eq!(delivered, 3);
    assert!(tracker.any_attached_alive_with(pid(500), &effects));

    let daemons_only = tracking([31, 33]);
    assert!(!daemons_only.any_attached_alive_with(pid(500), &effects));
}

#[test]
fn natural_completion_keeps_an_unreadable_process_attached_and_reports_it() {
    let effects = FakeEffects::new(|raw| match raw {
        40 => Err(InspectionError::Failed("ProcessIdentityUnavailable")),
        _ => Err(InspectionError::Denied("PermissionDenied")),
    });
    let tracker = tracking([40, 41]);
    let delivered = tracker.signal_attached_with(Signal::KILL, pid(500), &effects);
    assert!(effects.sent().is_empty());
    assert_eq!(delivered, 0);
    assert!(tracker.any_attached_alive_with(pid(500), &effects));
}

#[test]
fn session_inspection_separates_the_callers_session_from_a_new_one() {
    let own_session = proc_fs::capture_snapshot(getpid())
        .expect("the caller's stat")
        .session
        .expect("the caller's session");
    let mut detached = spawn_ready(
        "import os,sys,time; os.setsid(); sys.stdout.write('R'); sys.stdout.flush(); time.sleep(5)",
    );
    let detached_pid = Pid::from_child(&detached);
    let inspected = proc_fs::capture_snapshot(detached_pid).map(|snapshot| snapshot.session);
    let _ = detached.kill();
    let _ = detached.wait();
    assert_eq!(inspected, Ok(Some(detached_pid)));
    assert_eq!(
        completion_standing(own_session, Some(detached_pid)),
        CompletionStanding::Detached
    );
}

#[test]
fn tracked_identity_distinguishes_process_instances() {
    assert_eq!(identity(42), identity(42));
    assert_ne!(identity(42), identity(43));
}

#[test]
fn zombie_snapshots_are_terminal_process_state() {
    let live = snapshot(1);
    let zombie = ProcessSnapshot {
        zombie: true,
        ..live
    };
    assert!(snapshot_is_alive(live));
    assert!(!snapshot_is_alive(zombie));
}

#[test]
fn a_tracked_pid_with_another_identity_is_never_signalled() {
    let mut live =
        spawn_ready("import sys,time; sys.stdout.write('R'); sys.stdout.flush(); time.sleep(30)");
    let live_pid = Pid::from_child(&live);
    let actual = proc_fs::capture_snapshot(live_pid).expect("the live process");
    let reused = Tracker {
        root: Some(TrackedProcess {
            pid: live_pid,
            identity: identity(actual.identity.start_ticks + 1),
        }),
        processes: Vec::new(),
    };
    let delivered = reused.signal_all(Signal::KILL);
    let still_running = live.try_wait().expect("poll the live process").is_none();
    let _ = live.kill();
    let _ = live.wait();
    assert_eq!(delivered, 0);
    assert!(!reused.any_alive());
    assert!(still_running);
}

#[test]
fn a_refresh_tracks_descendants_that_left_the_session_and_signals_them() {
    let mut command = spawn_ready(
        "import os,sys,time\n\
         if os.fork() == 0:\n\
         \x20   os.setsid()\n\
         \x20   if os.fork() == 0:\n\
         \x20       time.sleep(30)\n\
         \x20   time.sleep(30)\n\
         \x20   os._exit(0)\n\
         sys.stdout.write('R'); sys.stdout.flush(); time.sleep(30)",
    );
    let root = Pid::from_child(&command);
    let mut tracker = Tracker::default();
    tracker.track_root(root).expect("record the command");
    let mut descendants = 0;
    for _ in 0..1000 {
        tracker.refresh().expect("inspect the command tree");
        descendants = tracker.processes.len();
        if descendants == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let delivered = tracker.signal_all(Signal::KILL);
    let _ = command.wait();
    assert_eq!(descendants, 2);
    assert_eq!(delivered, 3);
    for _ in 0..1000 {
        if !tracker.any_alive() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("killed descendants are still alive");
}

fn spawn_ready(script: &str) -> Child {
    let mut child = Command::new("python3")
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start python3");
    let mut ready = [0; 1];
    child
        .stdout
        .take()
        .expect("the child's stdout")
        .read_exact(&mut ready)
        .expect("the child reports readiness");
    assert_eq!(&ready, b"R");
    child
}
