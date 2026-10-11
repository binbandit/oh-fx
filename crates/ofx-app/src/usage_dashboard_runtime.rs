use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_contract::{UiEvent, UsageMenuUpdate, UsageReport, UsageScope};
use ofx_session::{ProfilePublisher, UsageRecovery};

use crate::app_agent_runtime::Emit;

const LOAD_THREAD: &str = "usage-dashboard";

pub(crate) struct UsageDashboard {
    shared: Arc<Shared>,
    ledger: Option<Ledger>,
    loader: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct Ledger {
    profile: ProfilePublisher,
    data_dir: PathBuf,
}

struct Shared {
    board: Mutex<Board>,
    cancel: AtomicBool,
    emit: Emit,
}

#[derive(Default)]
struct Board {
    loading: bool,
    reports: Option<[UsageReport; 3]>,
    shown: Option<UsageScope>,
}

impl Board {
    fn cached(&self, scope: UsageScope) -> Option<Box<UsageReport>> {
        self.reports
            .as_ref()?
            .iter()
            .find(|report| report.scope == scope)
            .map(|report| Box::new(report.clone()))
    }
}

impl Shared {
    fn board(&self) -> MutexGuard<'_, Board> {
        self.board.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn show(&self, update: UsageMenuUpdate) {
        (self.emit)(UiEvent::UsageMenu { update });
    }

    fn load(&self, ledger: &Ledger, snapshot_time_ms: i64) {
        let loaded = UsageRecovery::collect_unless_cancelled(&ledger.data_dir, &self.cancel)
            .filter(|_| !self.cancel.load(Ordering::SeqCst))
            .map(|recovery| ledger.profile.rolling_reports(snapshot_time_ms, &recovery));
        let mut board = self.board();
        board.loading = false;
        let Some(loaded) = loaded.filter(|_| !self.cancel.load(Ordering::SeqCst)) else {
            return;
        };
        let failed = loaded.is_err();
        if let Ok(reports) = loaded {
            board.reports = Some(reports);
        }
        let Some(scope) = board.shown.filter(|scope| *scope != UsageScope::Session) else {
            return;
        };
        match board.cached(scope) {
            Some(report) if !failed => self.show(UsageMenuUpdate::Report(report)),
            _ => self.show(UsageMenuUpdate::Failed(scope)),
        }
    }
}

impl UsageDashboard {
    pub(crate) fn new(emit: Emit) -> Self {
        Self {
            shared: Arc::new(Shared {
                board: Mutex::new(Board::default()),
                cancel: AtomicBool::new(false),
                emit,
            }),
            ledger: None,
            loader: None,
        }
    }

    pub(crate) fn open(&mut self, data_dir: Option<&Path>) {
        let scope = UsageScope::Days30;
        {
            let mut board = self.shared.board();
            board.shown = Some(scope);
            let report = board.cached(scope);
            self.shared.show(UsageMenuUpdate::Opened { scope, report });
        }
        self.request_refresh(scope, data_dir);
    }

    pub(crate) fn refresh(
        &mut self,
        scope: UsageScope,
        data_dir: Option<&Path>,
        session: impl FnOnce() -> Option<UsageReport>,
    ) {
        {
            let mut board = self.shared.board();
            board.shown = Some(scope);
            if scope == UsageScope::Session {
                let update = session().map_or(UsageMenuUpdate::Failed(scope), |report| {
                    UsageMenuUpdate::Report(Box::new(report))
                });
                return self.shared.show(update);
            }
            if let Some(report) = board.cached(scope) {
                return self.shared.show(UsageMenuUpdate::Report(report));
            }
            self.shared.show(UsageMenuUpdate::Loading(scope));
        }
        self.request_refresh(scope, data_dir);
    }

    pub(crate) fn reload(
        &mut self,
        scope: UsageScope,
        data_dir: Option<&Path>,
        session: impl FnOnce() -> Option<UsageReport>,
    ) {
        if scope == UsageScope::Session {
            return self.refresh(scope, data_dir, session);
        }
        self.shared.board().shown = Some(scope);
        self.request_refresh(scope, data_dir);
    }

    fn request_refresh(&mut self, scope: UsageScope, data_dir: Option<&Path>) {
        let Some(ledger) = self.ledger(data_dir) else {
            return self.shared.show(UsageMenuUpdate::Failed(scope));
        };
        {
            let mut board = self.shared.board();
            if board.loading {
                return;
            }
            board.loading = true;
        }
        if let Some(finished) = self.loader.take() {
            let _ = finished.join();
        }
        self.shared.cancel.store(false, Ordering::SeqCst);
        let shared = Arc::clone(&self.shared);
        let snapshot_time_ms = now_ms();
        let spawned = thread::Builder::new()
            .name(LOAD_THREAD.to_owned())
            .spawn(move || shared.load(&ledger, snapshot_time_ms));
        if let Ok(loader) = spawned {
            self.loader = Some(loader);
        } else {
            self.shared.board().loading = false;
            self.shared.show(UsageMenuUpdate::Failed(scope));
        }
    }

    fn ledger(&mut self, data_dir: Option<&Path>) -> Option<Ledger> {
        if self.ledger.is_none() {
            let data_dir = data_dir?;
            self.ledger = ProfilePublisher::open(data_dir).map(|profile| Ledger {
                profile,
                data_dir: data_dir.to_path_buf(),
            });
        }
        self.ledger.clone()
    }
}

impl Drop for UsageDashboard {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::SeqCst);
        if let Some(ledger) = &self.ledger {
            ledger.profile.abandon_for_process_exit();
        }
        if let Some(loader) = self.loader.take() {
            let _ = loader.join();
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
