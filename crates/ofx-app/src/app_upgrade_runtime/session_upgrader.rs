use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ofx_contract::BoxFuture;
use ofx_upgrade::UpgradeControl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpgradeState {
    Idle,
    Checking,
    Waiting,
    Downloading,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckOutcome {
    Current,
    Installed,
    Failed,
    Stopped,
}

pub(crate) trait ReleaseCheck: Send + 'static {
    fn check<'a>(
        &'a mut self,
        control: &'a UpgradeControl,
        found: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, CheckOutcome>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub(crate) initial_delay: Duration,
    pub(crate) interval: Duration,
}

const MAX_VERSION_LABEL_BYTES: usize = 32;
const THREAD_NAME: &str = "oh-fx-upgrade";

struct Status {
    state: UpgradeState,
    latest: String,
    label: String,
}

impl Status {
    fn label(&self) -> String {
        match self.state {
            UpgradeState::Downloading => format!("upgrading to {}...", self.latest),
            UpgradeState::Ready => "update ready: ctrl+g to reload".to_owned(),
            UpgradeState::Failed => "upgrade failed".to_owned(),
            UpgradeState::Idle | UpgradeState::Checking | UpgradeState::Waiting => String::new(),
        }
    }
}

struct Reporter<P> {
    status: Arc<Mutex<Status>>,
    publish: P,
}

impl<P: Fn(String)> Reporter<P> {
    fn set(&self, state: UpgradeState) {
        self.update(|status| status.state = state);
    }

    fn found(&self, version: &str) {
        let version = ofx_upgrade::normalize_version(version);
        let mut end = version.len().min(MAX_VERSION_LABEL_BYTES);
        while !version.is_char_boundary(end) {
            end -= 1;
        }
        let latest = version[..end].to_owned();
        self.update(|status| {
            status.latest = latest;
            status.state = UpgradeState::Downloading;
        });
    }

    fn update(&self, change: impl FnOnce(&mut Status)) {
        let label = {
            let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
            change(&mut status);
            let label = status.label();
            if label == status.label {
                return;
            }
            status.label.clone_from(&label);
            label
        };
        (self.publish)(label);
    }
}

pub(crate) struct SessionUpgrader {
    control: Arc<UpgradeControl>,
    thread: Option<JoinHandle<()>>,
}

impl SessionUpgrader {
    pub(crate) fn start(
        mut check: impl ReleaseCheck,
        timing: Timing,
        publish: impl Fn(String) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let control = Arc::new(UpgradeControl::new());
        let status = Arc::new(Mutex::new(Status {
            state: UpgradeState::Idle,
            latest: String::new(),
            label: String::new(),
        }));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let reporter = Reporter { status, publish };
        let worker_control = Arc::clone(&control);
        let thread = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || {
                runtime.block_on(run(&mut check, &worker_control, timing, &reporter));
            })?;
        Ok(Self {
            control,
            thread: Some(thread),
        })
    }

    pub(crate) fn stop_for_process_exit(&mut self) {
        self.control.stop_for_process_exit();
        self.thread.take();
    }
}

async fn run<P: Fn(String) + Sync>(
    check: &mut impl ReleaseCheck,
    control: &UpgradeControl,
    timing: Timing,
    reporter: &Reporter<P>,
) {
    if control
        .unless_stopped(tokio::time::sleep(timing.initial_delay))
        .await
        .is_err()
    {
        return;
    }
    loop {
        if control.stop_requested() {
            return;
        }
        reporter.set(UpgradeState::Checking);
        let mut found = |version: &str| reporter.found(version);
        let outcome = check.check(control, &mut found).await;
        match outcome {
            CheckOutcome::Installed => {
                reporter.set(UpgradeState::Ready);
                return;
            }
            CheckOutcome::Stopped => return,
            CheckOutcome::Failed => reporter.set(UpgradeState::Failed),
            CheckOutcome::Current => reporter.set(UpgradeState::Waiting),
        }
        if control
            .unless_stopped(tokio::time::sleep(timing.interval))
            .await
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
