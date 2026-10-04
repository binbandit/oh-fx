use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::Instant;

use super::*;

const WAIT: Duration = Duration::from_secs(5);
const QUICK: Timing = Timing {
    initial_delay: Duration::from_millis(1),
    interval: Duration::from_millis(1),
};

enum Step {
    Current,
    Found(&'static str, CheckOutcome),
    Hold,
}

struct Scripted {
    steps: VecDeque<Step>,
    checks: mpsc::Sender<()>,
}

impl ReleaseCheck for Scripted {
    fn check<'a>(
        &'a mut self,
        control: &'a UpgradeControl,
        found: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, CheckOutcome> {
        let step = self.steps.pop_front();
        let _ = self.checks.send(());
        Box::pin(async move {
            match step {
                Some(Step::Current) => CheckOutcome::Current,
                Some(Step::Found(version, outcome)) => {
                    found(version);
                    outcome
                }
                Some(Step::Hold) | None => {
                    match control.unless_stopped(std::future::pending::<()>()).await {
                        Ok(()) | Err(_) => CheckOutcome::Stopped,
                    }
                }
            }
        })
    }
}

struct Started {
    upgrader: SessionUpgrader,
    labels: mpsc::Receiver<String>,
    checks: mpsc::Receiver<()>,
}

fn start(steps: Vec<Step>, timing: Timing) -> Started {
    let (label_tx, labels) = mpsc::channel();
    let (check_tx, checks) = mpsc::channel();
    let upgrader = SessionUpgrader::start(
        Scripted {
            steps: steps.into(),
            checks: check_tx,
        },
        timing,
        move |label| {
            let _ = label_tx.send(label);
        },
    )
    .unwrap();
    Started {
        upgrader,
        labels,
        checks,
    }
}

fn next_label(labels: &mpsc::Receiver<String>) -> String {
    labels.recv_timeout(WAIT).expect("a label")
}

#[test]
fn a_found_release_reports_upgrading_then_ready_and_stops_checking() {
    let started = start(
        vec![
            Step::Current,
            Step::Found("v0.3.0", CheckOutcome::Installed),
        ],
        QUICK,
    );
    assert_eq!(next_label(&started.labels), "upgrading to 0.3.0...");
    assert_eq!(
        next_label(&started.labels),
        "update ready: ctrl+g to reload"
    );
    assert!(started.upgrader.readiness().ready());
    thread::sleep(Duration::from_millis(20));
    assert_eq!(started.checks.try_iter().count(), 2);
}

#[test]
fn a_failed_install_reports_upgrade_failed_until_the_next_check_clears_it() {
    let started = start(
        vec![
            Step::Found("1.2.3", CheckOutcome::Failed),
            Step::Current,
            Step::Hold,
        ],
        QUICK,
    );
    assert_eq!(next_label(&started.labels), "upgrading to 1.2.3...");
    assert_eq!(next_label(&started.labels), "upgrade failed");
    assert!(!started.upgrader.readiness().ready());
    assert_eq!(next_label(&started.labels), "");
}

#[test]
fn releases_that_are_current_publish_nothing() {
    let started = start(vec![Step::Current, Step::Current, Step::Hold], QUICK);
    for _ in 0..3 {
        started.checks.recv_timeout(WAIT).expect("a check");
    }
    assert!(started.labels.try_recv().is_err());
}

#[test]
fn the_version_label_drops_its_v_and_keeps_32_bytes() {
    let started = start(
        vec![Step::Found(
            "v1234567890123456789012345678901234567890",
            CheckOutcome::Installed,
        )],
        QUICK,
    );
    assert_eq!(
        next_label(&started.labels),
        "upgrading to 12345678901234567890123456789012..."
    );
}

#[test]
fn a_process_exit_stop_ends_the_wait_before_the_first_check() {
    let started = start(
        vec![Step::Current],
        Timing {
            initial_delay: Duration::from_mins(10),
            interval: Duration::from_mins(10),
        },
    );
    let stopping = Instant::now();
    started.upgrader.stop_for_process_exit();
    assert!(stopping.elapsed() < WAIT);
    thread::sleep(Duration::from_millis(20));
    assert!(started.checks.try_recv().is_err());
    assert!(started.labels.try_recv().is_err());
}

#[test]
fn a_process_exit_stop_ends_a_check_under_way() {
    let started = start(vec![Step::Hold], QUICK);
    started.checks.recv_timeout(WAIT).expect("a check");
    let stopping = Instant::now();
    started.upgrader.stop_for_process_exit();
    assert!(stopping.elapsed() < WAIT);
    thread::sleep(Duration::from_millis(20));
    assert!(started.labels.try_recv().is_err());
    assert!(started.checks.try_recv().is_err());
}
