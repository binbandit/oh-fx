use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use ofx_config::PrivateDir;
use ofx_contract::UsageCoverage;

use super::*;

struct Profile {
    root: tempfile::TempDir,
}

impl Profile {
    fn with_fact() -> Self {
        let profile = Self {
            root: tempfile::tempdir().unwrap(),
        };
        let data = profile.data();
        fs::create_dir_all(&data).unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        let now = now_ms();
        let ledger = data.join("usage.jsonl");
        fs::write(
            &ledger,
            format!(
                "{{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":{}}}\n{{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"created_at_ms\":{},\"model\":\"provider/model\",\"input_tokens\":5,\"output_tokens\":2,\"cache_read_tokens\":1,\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"billable_web_search_calls\":0,\"total_cost\":0.25}}}}\n",
                now - 120_000,
                now - 60_000
            ),
        )
        .unwrap();
        fs::set_permissions(&ledger, fs::Permissions::from_mode(0o600)).unwrap();
        profile
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("oh-fx")
    }
}

struct Watched {
    dashboard: UsageDashboard,
    events: Arc<Mutex<Vec<UiEvent>>>,
}

impl Watched {
    fn new() -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&events);
        let emit: Emit = Arc::new(move |event| seen.lock().unwrap().push(event));
        Self {
            dashboard: UsageDashboard::new(emit),
            events,
        }
    }

    fn settle(&mut self) {
        if let Some(loader) = self.dashboard.loader.take() {
            loader.join().unwrap();
        }
    }

    fn updates(&self) -> Vec<UsageMenuUpdate> {
        self.events
            .lock()
            .unwrap()
            .drain(..)
            .map(|event| match event {
                UiEvent::UsageMenu { update } => update,
                other => panic!("unexpected event {other:?}"),
            })
            .collect()
    }
}

fn scope_and_tokens(update: &UsageMenuUpdate) -> (UsageScope, Option<u64>) {
    match update {
        UsageMenuUpdate::Report(report) => (
            report.scope,
            report.totals.map(|totals| totals.total_tokens),
        ),
        other => panic!("expected a report, got {other:?}"),
    }
}

fn session_report() -> Option<UsageReport> {
    ofx_session::fresh_session_report().ok()
}

#[test]
fn one_load_serves_every_rolling_scope_after_the_menu_opens_loading() {
    let profile = Profile::with_fact();
    let data = profile.data();
    let mut watched = Watched::new();
    watched.dashboard.open(Some(&data));
    watched.settle();
    let opened = watched.updates();
    assert_eq!(
        opened[0],
        UsageMenuUpdate::Opened {
            scope: UsageScope::Days30,
            report: None
        }
    );
    assert_eq!(scope_and_tokens(&opened[1]), (UsageScope::Days30, Some(7)));
    assert_eq!(opened.len(), 2);

    for scope in [UsageScope::Days7, UsageScope::Hours24] {
        watched.dashboard.refresh(scope, Some(&data), || None);
        assert!(watched.dashboard.loader.is_none());
        let shown = watched.updates();
        assert_eq!(shown.len(), 1);
        assert_eq!(scope_and_tokens(&shown[0]), (scope, Some(7)));
    }
}

#[test]
fn a_reopened_menu_shows_the_cached_reports_at_once_and_reloads_them() {
    let profile = Profile::with_fact();
    let data = profile.data();
    let mut watched = Watched::new();
    watched.dashboard.open(Some(&data));
    watched.settle();
    watched.updates();

    watched.dashboard.open(Some(&data));
    watched.settle();
    let reopened = watched.updates();
    let UsageMenuUpdate::Opened {
        scope: UsageScope::Days30,
        report: Some(cached),
    } = &reopened[0]
    else {
        panic!("expected the cached report, got {reopened:?}");
    };
    assert_eq!(cached.coverage, UsageCoverage::Partial);
    assert_eq!(
        scope_and_tokens(&reopened[1]),
        (UsageScope::Days30, Some(7))
    );

    watched
        .dashboard
        .reload(UsageScope::Days7, Some(&data), || None);
    watched.settle();
    let reloaded = watched.updates();
    assert_eq!(reloaded.len(), 1);
    assert_eq!(scope_and_tokens(&reloaded[0]), (UsageScope::Days7, Some(7)));
}

#[test]
fn the_session_scope_comes_from_the_session_and_fails_without_one() {
    let mut watched = Watched::new();
    watched
        .dashboard
        .refresh(UsageScope::Session, None, session_report);
    watched
        .dashboard
        .reload(UsageScope::Session, None, session_report);
    watched
        .dashboard
        .refresh(UsageScope::Session, None, || None);
    let updates = watched.updates();
    assert_eq!(
        scope_and_tokens(&updates[0]),
        (UsageScope::Session, Some(0))
    );
    assert_eq!(
        scope_and_tokens(&updates[1]),
        (UsageScope::Session, Some(0))
    );
    assert_eq!(updates[2], UsageMenuUpdate::Failed(UsageScope::Session));
    assert!(watched.dashboard.loader.is_none());
}

#[test]
fn without_a_profile_each_rolling_scope_reports_its_failure() {
    let mut watched = Watched::new();
    watched.dashboard.open(None);
    watched.dashboard.refresh(UsageScope::Days7, None, || None);
    watched.dashboard.reload(UsageScope::Hours24, None, || None);
    assert_eq!(
        watched.updates(),
        [
            UsageMenuUpdate::Opened {
                scope: UsageScope::Days30,
                report: None
            },
            UsageMenuUpdate::Failed(UsageScope::Days30),
            UsageMenuUpdate::Loading(UsageScope::Days7),
            UsageMenuUpdate::Failed(UsageScope::Days7),
            UsageMenuUpdate::Failed(UsageScope::Hours24),
        ]
    );
}

#[test]
fn a_finished_load_shows_only_a_rolling_scope_and_a_cancelled_one_shows_nothing() {
    let profile = Profile::with_fact();
    let ledger = Ledger {
        profile: ProfilePublisher::open(&profile.data()).unwrap(),
        data_dir: profile.data(),
    };
    let watched = Watched::new();
    let shared = &watched.dashboard.shared;

    shared.cancel.store(true, Ordering::SeqCst);
    shared.board().loading = true;
    shared.load(&ledger, now_ms());
    assert!(!shared.board().loading);
    assert!(shared.board().reports.is_none());

    shared.cancel.store(false, Ordering::SeqCst);
    shared.board().shown = Some(UsageScope::Session);
    shared.load(&ledger, now_ms());
    assert!(shared.board().reports.is_some());
    assert!(watched.updates().is_empty());
}

#[test]
fn dropping_the_dashboard_abandons_a_load_waiting_on_the_ledger_lock() {
    let profile = Profile::with_fact();
    let data = profile.data();
    let held = PrivateDir::open_existing(&data)
        .unwrap()
        .unwrap()
        .try_lock("usage.lock")
        .unwrap()
        .unwrap();
    let Watched {
        mut dashboard,
        events,
    } = Watched::new();
    dashboard.open(Some(&data));
    thread::sleep(Duration::from_millis(100));
    let dropping = Instant::now();
    drop(dashboard);
    assert!(
        dropping.elapsed() < Duration::from_secs(1),
        "{:?}",
        dropping.elapsed()
    );
    let updates: Vec<UiEvent> = events.lock().unwrap().drain(..).collect();
    assert_eq!(
        updates,
        [UiEvent::UsageMenu {
            update: UsageMenuUpdate::Opened {
                scope: UsageScope::Days30,
                report: None
            }
        }]
    );
    drop(held);
}
