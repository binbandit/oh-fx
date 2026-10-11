use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};

use super::*;

const OWNER: &str = "owner-session";
const NOW: i64 = 1_000_000;

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    store: TerminalStore,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap().join("terminal");
        let store = TerminalStore::new(&root).unwrap();
        Self {
            _directory: directory,
            root,
            store,
        }
    }

    fn create(&self, timeout_ms: Option<u64>) -> DurableTerminal {
        self.store
            .create(
                &NewTerminal {
                    owner_session_id: OWNER,
                    shell: "/bin/bash",
                    cwd: "/workspace",
                    command: "printf ok",
                    timeout_ms,
                    dimensions: Dimensions {
                        rows: 24,
                        columns: 80,
                    },
                },
                NOW,
            )
            .unwrap()
    }

    fn record_path(&self, session_id: &str) -> PathBuf {
        self.root
            .join(OWNER)
            .join(format!("record-{session_id}.json"))
    }

    fn saved(&self, session_id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.record_path(session_id)).unwrap()).unwrap()
    }

    fn write_raw(&self, session_id: &str, record: &Value) {
        let path = self.record_path(session_id);
        fs::write(&path, record.to_string()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn created_records_are_private_and_keep_upstreams_field_names() {
    let fixture = Fixture::new();
    let terminal = fixture.create(None);
    let id = &terminal.record().session_id;
    let suffix = id.strip_prefix("shell-").unwrap();
    assert_eq!(suffix.len(), 22);
    assert!(
        suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    );
    assert_eq!(mode(&fixture.root.join(OWNER)), 0o700);
    assert_eq!(mode(&fixture.record_path(id)), 0o600);
    let saved = fixture.saved(id);
    let host = saved["host_identity"].as_str().unwrap().to_owned();
    let instance = host
        .strip_prefix("oh-fx-process-v1/")
        .and_then(|rest| rest.strip_suffix(&format!("/{}", std::process::id())))
        .unwrap();
    assert_eq!(instance.len(), 32);
    assert!(instance.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(
        saved,
        json!({
            "schema_version": 2,
            "session_id": id,
            "owner_session_id": OWNER,
            "host_identity": host,
            "shell": "/bin/bash",
            "cwd": "/workspace",
            "command": "printf ok",
            "timeout_ms": null,
            "timeout_at_ms": null,
            "timed_out": false,
            "lifecycle": "starting",
            "dimensions": {"rows": 24, "columns": 80},
            "pid": null,
            "termination": null,
            "created_at_ms": NOW,
            "updated_at_ms": NOW,
        })
    );
    let reopened = fixture.store.open(OWNER, id).unwrap();
    assert_eq!(reopened.record(), terminal.record());
    assert_ne!(
        fixture.create(None).record().session_id,
        terminal.record().session_id
    );
}

#[test]
fn lifecycle_changes_are_saved_in_order() {
    let fixture = Fixture::new();
    let mut terminal = fixture.create(Some(5_000));
    terminal.mark_started(4242, NOW + 10).unwrap();
    assert_eq!(terminal.record().lifecycle, Lifecycle::Running);
    assert_eq!(terminal.record().pid.as_deref(), Some("4242"));
    assert_eq!(terminal.record().timeout_at_ms, Some(NOW + 5_010));
    assert_eq!(
        terminal.mark_started(4343, NOW + 20),
        Err(StoreError::InvalidLifecycle)
    );
    terminal.mark_timed_out(NOW + 5_010).unwrap();
    terminal.mark_timed_out(NOW + 5_020).unwrap();
    assert!(terminal.record().timed_out);
    assert_eq!(terminal.record().updated_at_ms, NOW + 5_010);
    terminal
        .persist_termination(Termination::Signal(9), NOW + 5_030)
        .unwrap();
    assert_eq!(terminal.record().lifecycle, Lifecycle::Exited);
    assert_eq!(terminal.termination(), Some(Termination::Signal(9)));
    let id = terminal.record().session_id.clone();
    let saved = fixture.saved(&id);
    assert_eq!(saved["termination"], json!({"signal": 9}));
    assert_eq!(saved["lifecycle"], "exited");
    terminal.persist_lost(NOW + 5_040).unwrap();
    assert_eq!(terminal.record().lifecycle, Lifecycle::Exited);
    terminal.close(NOW + 5_050).unwrap();
    assert_eq!(fixture.saved(&id)["lifecycle"], "closed");
    let closed = terminal.record().clone();
    for refused in [
        terminal.persist_termination(Termination::Exited(0), NOW + 6_000),
        terminal.persist_lost(NOW + 6_000),
        terminal.close(NOW + 6_000),
    ] {
        assert_eq!(refused, Err(StoreError::InvalidLifecycle));
    }
    assert_eq!(terminal.record(), &closed);
    assert_eq!(fixture.store.open(OWNER, &id).unwrap().record(), &closed);
}

#[test]
fn deadlines_and_terminations_are_validated() {
    let fixture = Fixture::new();
    let mut untimed = fixture.create(None);
    untimed.mark_started(1, NOW).unwrap();
    assert_eq!(
        untimed.mark_timed_out(NOW),
        Err(StoreError::InvalidLifecycle)
    );
    for invalid in [
        Termination::Exited(-1),
        Termination::Exited(256),
        Termination::Signal(0),
        Termination::Signal(256),
    ] {
        assert_eq!(
            untimed.persist_termination(invalid, NOW),
            Err(StoreError::InvalidLifecycle)
        );
    }
    untimed.persist_lost(NOW + 1).unwrap();
    assert_eq!(untimed.record().lifecycle, Lifecycle::Lost);
    let mut overflowing = fixture.create(Some(u64::MAX));
    assert_eq!(
        overflowing.mark_started(1, NOW),
        Err(StoreError::CapacityExceeded)
    );
    assert_eq!(overflowing.record().lifecycle, Lifecycle::Starting);
}

#[test]
fn reconciling_marks_only_other_instances_live_terminals() {
    let fixture = Fixture::new();
    let mut ours = fixture.create(None);
    ours.mark_started(1, NOW).unwrap();
    let mut finished = fixture.create(None);
    finished.mark_started(2, NOW).unwrap();
    finished
        .persist_termination(Termination::Exited(3), NOW + 1)
        .unwrap();
    let starting = fixture.create(None);
    let mut terminated = fixture.create(None);
    terminated.mark_started(3, NOW).unwrap();
    let mut raw = fixture.saved(&terminated.record().session_id);
    raw["termination"] = json!({"exited": 0});
    fixture.write_raw(&terminated.record().session_id, &raw);
    fixture.write_raw("shell-corrupt", &json!({"schema_version": 2}));

    let reconciled = |store: &TerminalStore| -> Vec<(String, Lifecycle)> {
        let mut records: Vec<_> = store
            .reconcile_owner(OWNER, NOW + 100)
            .unwrap()
            .into_iter()
            .map(|record| (record.session_id, record.lifecycle))
            .collect();
        records.sort_by(|left, right| left.0.cmp(&right.0));
        records
    };
    let expected = |lifecycles: [Lifecycle; 4]| {
        let mut records: Vec<_> = [&ours, &finished, &starting, &terminated]
            .into_iter()
            .map(|terminal| terminal.record().session_id.clone())
            .zip(lifecycles)
            .collect();
        records.sort_by(|left, right| left.0.cmp(&right.0));
        records
    };
    assert_eq!(
        reconciled(&fixture.store),
        expected([
            Lifecycle::Running,
            Lifecycle::Exited,
            Lifecycle::Starting,
            Lifecycle::Exited,
        ])
    );
    let later = TerminalStore::new(&fixture.root).unwrap();
    assert_eq!(
        reconciled(&later),
        expected([
            Lifecycle::Lost,
            Lifecycle::Exited,
            Lifecycle::Lost,
            Lifecycle::Exited,
        ])
    );
    let saved = fixture.saved(&ours.record().session_id);
    assert_eq!(saved["lifecycle"], "lost");
    assert_eq!(saved["updated_at_ms"], NOW + 100);
    assert_eq!(
        fixture.saved(&finished.record().session_id)["updated_at_ms"],
        NOW + 1
    );
}

#[test]
fn owners_and_terminals_need_safe_ids() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.store.reconcile_owner(OWNER, NOW).unwrap(),
        Vec::new()
    );
    for owner in ["", ".", "..", "../escape", "nested/owner", "V2"] {
        assert_eq!(
            fixture.store.reconcile_owner(owner, NOW),
            Err(StoreError::InvalidSessionId),
            "{owner}"
        );
    }
    let terminal = fixture.create(None);
    for session_id in ["", "../record", "a/b"] {
        assert_eq!(
            fixture.store.open(OWNER, session_id).err(),
            Some(StoreError::InvalidSessionId)
        );
    }
    assert_eq!(
        fixture.store.open(OWNER, "shell-missing").err(),
        Some(StoreError::RecordNotFound)
    );
    assert_eq!(
        fixture
            .store
            .open("other-owner", &terminal.record().session_id)
            .err(),
        Some(StoreError::RecordNotFound)
    );
}

#[test]
fn records_that_break_their_contract_are_rejected() {
    let fixture = Fixture::new();
    let terminal = fixture.create(None);
    let id = terminal.record().session_id.clone();
    let valid = fixture.saved(&id);
    for (field, value) in [
        ("schema_version", json!(3)),
        ("cwd", json!("relative")),
        ("shell", json!("")),
        ("timed_out", json!(true)),
        ("timeout_at_ms", json!(NOW)),
        ("termination", json!({"signal": 0})),
        ("dimensions", json!({"rows": 0, "columns": 80})),
        ("lifecycle", json!("paused")),
        ("session_id", json!("shell-other")),
        ("owner_session_id", json!("other-owner")),
        ("unknown", json!(1)),
    ] {
        let mut broken = valid.clone();
        broken[field] = value;
        fixture.write_raw(&id, &broken);
        assert!(
            matches!(
                fixture.store.open(OWNER, &id).err(),
                Some(StoreError::InvalidRecord)
            ),
            "{field}"
        );
    }
}
