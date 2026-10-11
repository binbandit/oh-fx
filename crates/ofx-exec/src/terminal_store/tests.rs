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
    assert_eq!(mode(&fixture.root), 0o700);
    assert_eq!(mode(&fixture.root.join(OWNER)), 0o700);
    assert_eq!(mode(&fixture.record_path(id)), 0o600);
    assert!(fs::read(fixture.record_path(id)).unwrap().ends_with(b"}\n"));
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
    assert_eq!(terminal.record().updated_at_ms, NOW + 5_040);
    terminal.close(NOW + 5_050).unwrap();
    assert_eq!(fixture.saved(&id)["lifecycle"], "closed");
    terminal.persist_lost(NOW + 6_000).unwrap();
    terminal
        .persist_termination(Termination::Exited(0), NOW + 6_010)
        .unwrap();
    assert_eq!(terminal.record().lifecycle, Lifecycle::Closed);
    assert_eq!(terminal.termination(), Some(Termination::Exited(0)));
    assert_eq!(
        terminal.close(NOW + 6_020),
        Err(StoreError::InvalidLifecycle)
    );
    assert_eq!(
        fixture.store.open(OWNER, &id).unwrap().record(),
        terminal.record()
    );
}

#[test]
fn a_failed_save_leaves_the_record_and_its_file_unchanged() {
    let fixture = Fixture::new();
    let mut terminal = fixture.create(None);
    let id = terminal.record().session_id.clone();
    let path = fixture.record_path(&id);
    let before = fs::read(&path).unwrap();
    let kept = terminal.record().clone();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    assert_eq!(
        terminal.mark_started(7, NOW + 1),
        Err(StoreError::Durable(DurableError::AccessDenied))
    );
    assert_eq!(terminal.record(), &kept);
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn an_unreleased_start_rolls_its_record_back() {
    let fixture = Fixture::new();
    let unreleased = fixture.create(None);
    let id = unreleased.record().session_id.clone();
    unreleased.rollback_unreleased_start().unwrap();
    assert!(!fixture.record_path(&id).exists());
    assert_eq!(
        fixture.store.open(OWNER, &id).err(),
        Some(StoreError::RecordNotFound)
    );
    let mut started = fixture.create(None);
    started.mark_started(1, NOW).unwrap();
    let id = started.record().session_id.clone();
    assert_eq!(
        started.rollback_unreleased_start(),
        Err(StoreError::InvalidLifecycle)
    );
    assert!(fixture.record_path(&id).exists());
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
            Err(StoreError::InvalidRecord)
        );
    }
    untimed.persist_lost(NOW + 1).unwrap();
    assert_eq!(untimed.record().lifecycle, Lifecycle::Lost);
    for timeout in [u64::MAX, u64::try_from(i64::MAX).unwrap()] {
        let mut overflowing = fixture.create(Some(timeout));
        assert_eq!(
            overflowing.mark_started(1, NOW),
            Err(StoreError::CapacityExceeded)
        );
        assert_eq!(overflowing.record().lifecycle, Lifecycle::Starting);
    }
}

fn foreign(fixture: &Fixture, session_id: &str, created_at_ms: i64, changes: &Value) -> Value {
    let mut record = fixture.saved(&fixture.create(None).record().session_id);
    let original = record["session_id"].as_str().unwrap().to_owned();
    fs::remove_file(fixture.record_path(&original)).unwrap();
    record["session_id"] = json!(session_id);
    record["host_identity"] = json!("oh-fx-process-v1/00000000000000000000000000000000/1");
    record["created_at_ms"] = json!(created_at_ms);
    for (field, value) in changes.as_object().unwrap() {
        record[field] = value.clone();
    }
    fixture.write_raw(session_id, &record);
    record
}

#[test]
fn reconciling_marks_only_terminals_of_other_processes_lost() {
    let fixture = Fixture::new();
    let mut ours = fixture.create(None);
    ours.mark_started(1, NOW).unwrap();
    foreign(
        &fixture,
        "shell-running",
        NOW - 3,
        &json!({"lifecycle": "running", "pid": "2"}),
    );
    foreign(&fixture, "shell-starting", NOW - 2, &json!({}));
    foreign(
        &fixture,
        "shell-finished",
        NOW - 1,
        &json!({"lifecycle": "running", "pid": "3", "termination": {"exited": 4}}),
    );
    let exited = foreign(
        &fixture,
        "shell-exited",
        NOW - 4,
        &json!({"lifecycle": "exited", "termination": {"signal": 9}}),
    );
    let closed = foreign(
        &fixture,
        "shell-closed",
        NOW - 5,
        &json!({"lifecycle": "closed"}),
    );
    foreign(
        &fixture,
        "shell-elsewhere",
        NOW,
        &json!({"owner_session_id": "other-owner"}),
    );
    let mismatched = foreign(&fixture, "shell-named", NOW, &json!({}));
    fs::rename(
        fixture.record_path("shell-named"),
        fixture.record_path("shell-renamed"),
    )
    .unwrap();
    fixture.write_raw("shell-corrupt", &json!({"schema_version": 2}));
    fs::write(
        fixture
            .root
            .join(OWNER)
            .join(".record-shell-temp.json.tmp.1"),
        mismatched.to_string(),
    )
    .unwrap();

    let records = fixture.store.reconcile_owner(OWNER, NOW + 100).unwrap();
    let summary: Vec<(&str, Lifecycle)> = records
        .iter()
        .map(|record| (record.session_id.as_str(), record.lifecycle))
        .collect();
    assert_eq!(
        summary,
        [
            ("shell-closed", Lifecycle::Closed),
            ("shell-exited", Lifecycle::Exited),
            ("shell-running", Lifecycle::Lost),
            ("shell-starting", Lifecycle::Lost),
            ("shell-finished", Lifecycle::Exited),
            (ours.record().session_id.as_str(), Lifecycle::Running),
        ]
    );
    assert_eq!(fixture.saved("shell-running")["updated_at_ms"], NOW + 100);
    assert_eq!(fixture.saved("shell-finished")["lifecycle"], "exited");
    assert_eq!(fixture.saved("shell-exited"), exited);
    assert_eq!(fixture.saved("shell-closed"), closed);
    assert_eq!(
        fixture.saved(&ours.record().session_id)["updated_at_ms"],
        NOW
    );
    let later = TerminalStore::new(&fixture.root).unwrap();
    assert_eq!(later.reconcile_owner(OWNER, NOW + 200).unwrap(), records);
}

#[test]
fn a_terminal_opens_only_for_the_session_that_owns_it() {
    let fixture = Fixture::new();
    let terminal = fixture.create(None);
    let id = terminal.record().session_id.clone();
    assert_eq!(
        fixture.store.open("other-owner", &id).err(),
        Some(StoreError::OwnedByAnotherSession)
    );
    let other = fixture.root.join("other-owner");
    fs::create_dir(&other).unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy(
        fixture.record_path(&id),
        other.join(format!("record-{id}.json")),
    )
    .unwrap();
    assert_eq!(
        fixture.store.open(OWNER, &id).err(),
        Some(StoreError::DuplicateTerminalSession)
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
        Some(StoreError::OwnedByAnotherSession)
    );
}

#[test]
fn records_that_break_their_contract_are_rejected() {
    let fixture = Fixture::new();
    let terminal = fixture.create(None);
    let id = terminal.record().session_id.clone();
    let valid = fixture.saved(&id);
    let mut unsupported = valid.clone();
    unsupported["schema_version"] = json!(3);
    fixture.write_raw(&id, &unsupported);
    assert_eq!(
        fixture.store.open(OWNER, &id).err(),
        Some(StoreError::UnsupportedSchema)
    );
    for (field, value) in [
        ("cwd", json!("relative")),
        ("host_identity", json!("")),
        ("termination", json!({"exited": 256})),
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
        if field == "timeout_at_ms" {
            broken["timeout_ms"] = json!(5);
            broken["timeout_at_ms"] = json!(NOW - 1);
        }
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
