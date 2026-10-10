use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::spawn_gate::hold_off_spawns;

const FACT_ID: &str = "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_ID: &str = "gen_01ARZ3NDEKTSV4RRFFQ69G5FAW";

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("data/oh-fx")
    }

    fn ledger(&self) -> PathBuf {
        self.data().join(USAGE_FILE)
    }

    fn store(&self) -> ProfileUsageStore {
        ProfileUsageStore::open(&self.data()).unwrap()
    }

    fn write(&self, bytes: &[u8]) {
        fs::create_dir_all(self.data()).unwrap();
        fs::set_permissions(self.data(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(self.ledger(), bytes).unwrap();
        fs::set_permissions(self.ledger(), fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.data())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn coverage(started_at_ms: i64) -> String {
    format!("{{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":{started_at_ms}}}\n")
}

fn generation(id: &str, created_at_ms: i64, input_tokens: u64) -> String {
    format!(
        "{{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{{\"id\":\"{id}\",\"created_at_ms\":{created_at_ms},\"model\":\"provider/model\",\"input_tokens\":{input_tokens},\"output_tokens\":2,\"cache_read_tokens\":1,\"cache_write_tokens\":0,\"reasoning_tokens\":1,\"billable_web_search_calls\":0,\"total_cost\":0.25}}}}\n"
    )
}

fn pending(id: &str, observed_at_ms: i64) -> String {
    format!(
        "{{\"schema_version\":1,\"kind\":\"pending\",\"id\":\"{id}\",\"observed_at_ms\":{observed_at_ms}}}\n"
    )
}

fn incident(occurred_at_ms: i64, completeness: &str) -> String {
    format!(
        "{{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":{occurred_at_ms},\"completeness\":\"{completeness}\"}}\n"
    )
}

fn fact(id: &str, created_at_ms: i64, input_tokens: u64) -> GenerationFact {
    GenerationFact {
        id: id.to_owned(),
        created_at_ms,
        model: "provider/model".to_owned(),
        input_tokens,
        output_tokens: 2,
        cache_read_tokens: 1,
        cache_write_tokens: 0,
        reasoning_tokens: Some(1),
        billable_web_search_calls: 0,
        total_cost: 0.25,
    }
}

#[test]
fn a_profile_without_a_ledger_loads_empty_and_creates_nothing() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    assert_eq!(store.load().unwrap(), LoadedUsage::default());
    assert!(!fixture.root.path().join("data").exists());

    fs::create_dir_all(fixture.data()).unwrap();
    fs::set_permissions(fixture.data(), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(store.load().unwrap(), LoadedUsage::default());
    assert!(fixture.entries().is_empty());
}

#[test]
fn a_ledger_fx_wrote_loads_record_by_record() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    fixture.write(
        [
            coverage(500),
            generation(FACT_ID, 1000, 10),
            pending(OTHER_ID, 1100),
            incident(1200, "pending"),
            incident(1300, "incomplete"),
            "{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAX\",\"created_at_ms\":1400,\"model\":\"codex/gpt-5\",\"input_tokens\":7,\"output_tokens\":3,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"total_cost\":0}}\n".to_owned(),
        ]
        .concat()
        .as_bytes(),
    );
    let loaded = store.load().unwrap();
    assert_eq!(loaded.coverage_started_at_ms, Some(500));
    assert_eq!(
        loaded.facts,
        [
            fact(FACT_ID, 1000, 10),
            GenerationFact {
                id: "gen_01ARZ3NDEKTSV4RRFFQ69G5FAX".to_owned(),
                created_at_ms: 1400,
                model: "codex/gpt-5".to_owned(),
                input_tokens: 7,
                output_tokens: 3,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
                billable_web_search_calls: 0,
                total_cost: 0.0,
            },
        ]
    );
    assert_eq!(
        loaded.pending,
        [PendingMarker {
            id: OTHER_ID.to_owned(),
            observed_at_ms: 1100,
        }]
    );
    assert_eq!(
        loaded.incidents,
        [
            UsageIncident {
                occurred_at_ms: 1200,
                completeness: UsageCompleteness::Pending,
            },
            UsageIncident {
                occurred_at_ms: 1300,
                completeness: UsageCompleteness::Incomplete,
            },
        ]
    );
    assert_eq!(fixture.entries(), [USAGE_FILE]);
}

#[test]
fn replayed_records_dedupe_and_one_conflicting_variant_is_kept() {
    let fixture = Fixture::new();
    fixture.write(
        [
            coverage(500),
            coverage(500),
            generation(FACT_ID, 1000, 10),
            generation(FACT_ID, 1000, 10),
            generation(FACT_ID, 1000, 11),
            generation(FACT_ID, 1000, 11),
            generation(FACT_ID, 1000, 12),
            pending(OTHER_ID, 1100),
            pending(OTHER_ID, 1100),
            pending(OTHER_ID, 1150),
            pending(OTHER_ID, 1175),
        ]
        .concat()
        .as_bytes(),
    );
    let loaded = fixture.store().load().unwrap();
    assert_eq!(
        loaded.facts,
        [fact(FACT_ID, 1000, 10), fact(FACT_ID, 1000, 11)]
    );
    assert_eq!(loaded.pending.len(), 2);
    assert_eq!(loaded.pending[1].observed_at_ms, 1150);
    assert_eq!(
        loaded.incidents,
        [UsageIncident {
            occurred_at_ms: 1150,
            completeness: UsageCompleteness::Incomplete,
        }]
    );
}

#[test]
fn records_without_their_coverage_or_with_a_second_one_are_invalid() {
    for ledger in [
        generation(FACT_ID, 1000, 10),
        pending(OTHER_ID, 1100),
        [coverage(500), coverage(501)].concat(),
    ] {
        let fixture = Fixture::new();
        fixture.write(ledger.as_bytes());
        assert_eq!(
            fixture.store().load(),
            Err(UsageStoreError::Invalid),
            "{ledger}"
        );
    }
    let fixture = Fixture::new();
    fixture.write(
        [incident(1200, "incomplete"), coverage(500)]
            .concat()
            .as_bytes(),
    );
    assert_eq!(fixture.store().load().unwrap().incidents.len(), 1);
}

#[test]
fn malformed_records_make_the_ledger_invalid() {
    let fact = "{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"created_at_ms\":1,\"model\":\"m\",\"input_tokens\":1,\"output_tokens\":1,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"total_cost\":0}";
    for line in [
        "[]".to_owned(),
        "not json".to_owned(),
        "{\"schema_version\":2,\"kind\":\"coverage\",\"started_at_ms\":1}".to_owned(),
        "{\"schema_version\":\"1\",\"kind\":\"coverage\",\"started_at_ms\":1}".to_owned(),
        "{\"schema_version\":1.0,\"kind\":\"coverage\",\"started_at_ms\":1}".to_owned(),
        "{\"kind\":\"coverage\",\"started_at_ms\":1}".to_owned(),
        "{\"schema_version\":1,\"kind\":\"usage\",\"started_at_ms\":1}".to_owned(),
        "{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":1,\"extra\":1}".to_owned(),
        "{\"schema_version\":1,\"kind\":\"coverage\",\"started_at\":1}".to_owned(),
        "{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":-1}".to_owned(),
        "{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":1,\"occurred_at_ms\":1,\"completeness\":\"incomplete\"}"
            .to_owned(),
        "{\"schema_version\":1,\"kind\":\"pending\",\"id\":\"resp_1\",\"observed_at_ms\":1}"
            .to_owned(),
        "{\"schema_version\":1,\"kind\":\"pending\",\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV\"}"
            .to_owned(),
        "{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":1,\"completeness\":\"legacy\"}"
            .to_owned(),
        "{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":1,\"completeness\":\"complete\"}"
            .to_owned(),
        "{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":1,\"completeness\":\"Incomplete\"}"
            .to_owned(),
        format!("{{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{fact},\"extra\":1}}"),
        format!(
            "{{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{}}}",
            fact.replace("\"model\":\"m\"", "\"model\":\"\"")
        ),
    ] {
        let fixture = Fixture::new();
        fixture.write(format!("{}{line}\n", coverage(0)).as_bytes());
        assert_eq!(
            fixture.store().load(),
            Err(UsageStoreError::Invalid),
            "{line}"
        );
    }
    let fixture = Fixture::new();
    fixture.write(
        format!(
            "{}\n{{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{fact}}}\r\n\n",
            coverage(0)
        )
        .as_bytes(),
    );
    assert_eq!(fixture.store().load().unwrap().facts.len(), 1);
}

#[test]
fn an_incomplete_tail_fails_the_load_and_is_left_for_the_writer() {
    let fixture = Fixture::new();
    let mut bytes = [coverage(500), generation(FACT_ID, 1000, 10)].concat();
    bytes.push_str("{\"schema_version\":1,\"kind\":\"pen");
    fixture.write(bytes.as_bytes());
    assert_eq!(fixture.store().load(), Err(UsageStoreError::Incomplete));
    assert_eq!(fs::read(fixture.ledger()).unwrap(), bytes.as_bytes());
}

#[test]
fn ledgers_over_their_record_and_byte_limits_are_refused() {
    let fixture = Fixture::new();
    let long_model = "m".repeat(MAX_RECORD_BYTES);
    fixture.write(
        [
            coverage(0),
            generation(FACT_ID, 1, 1).replace("provider/model", &long_model),
        ]
        .concat()
        .as_bytes(),
    );
    assert_eq!(
        fixture.store().load(),
        Err(UsageStoreError::CapacityExceeded)
    );

    let fixture = Fixture::new();
    fixture.write(b"");
    File::options()
        .write(true)
        .open(fixture.ledger())
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    assert_eq!(
        fixture.store().load(),
        Err(UsageStoreError::CapacityExceeded)
    );

    let fixture = Fixture::new();
    fixture.write(incident(1, "incomplete").repeat(MAX_RECORDS + 1).as_bytes());
    assert_eq!(
        fixture.store().load(),
        Err(UsageStoreError::CapacityExceeded)
    );
}

#[test]
fn a_large_ledger_loads_every_fact_in_file_order() {
    let fixture = Fixture::new();
    let mut ledger = coverage(0);
    let ids: Vec<String> = (0..20_000)
        .map(|index| format!("gen_{index:026}"))
        .collect();
    for (index, id) in ids.iter().enumerate() {
        ledger.push_str(&generation(id, i64::try_from(index).unwrap(), 4));
    }
    fixture.write(ledger.as_bytes());
    let loaded = fixture.store().load().unwrap();
    assert_eq!(loaded.facts.len(), ids.len());
    assert!(
        loaded
            .facts
            .iter()
            .zip(&ids)
            .all(|(fact, id)| fact.id == *id)
    );
}

#[test]
fn unsafe_profile_state_is_refused_without_repair() {
    let fixture = Fixture::new();
    fixture.write(coverage(0).as_bytes());
    fs::set_permissions(fixture.data(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        fixture.store().load(),
        Err(UsageStoreError::PermissionsUnsupported)
    );
    assert_eq!(mode(&fixture.data()), 0o755);

    let fixture = Fixture::new();
    fixture.write(coverage(0).as_bytes());
    fs::set_permissions(fixture.ledger(), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        fixture.store().load(),
        Err(UsageStoreError::PermissionsUnsupported)
    );
    assert_eq!(mode(&fixture.ledger()), 0o644);

    let fixture = Fixture::new();
    fixture.write(coverage(0).as_bytes());
    fs::hard_link(fixture.ledger(), fixture.root.path().join("linked")).unwrap();
    assert_eq!(fixture.store().load(), Err(UsageStoreError::PathUnsafe));

    let fixture = Fixture::new();
    fixture.write(coverage(0).as_bytes());
    fs::rename(fixture.ledger(), fixture.root.path().join("elsewhere")).unwrap();
    symlink(fixture.root.path().join("elsewhere"), fixture.ledger()).unwrap();
    assert_eq!(fixture.store().load(), Err(UsageStoreError::PathUnsafe));

    let fixture = Fixture::new();
    fixture.write(coverage(0).as_bytes());
    fs::remove_file(fixture.ledger()).unwrap();
    fs::create_dir(fixture.ledger()).unwrap();
    assert_eq!(fixture.store().load(), Err(UsageStoreError::PathUnsafe));

    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.path().join("real")).unwrap();
    fs::create_dir_all(fixture.root.path().join("data")).unwrap();
    symlink(fixture.root.path().join("real"), fixture.data()).unwrap();
    assert_eq!(
        ProfileUsageStore::open(&fixture.data()).err(),
        Some(UsageStoreError::PathUnsafe)
    );
}

#[test]
fn an_unsafe_lock_file_is_refused() {
    let fixture = Fixture::new();
    fixture.write(coverage(0).as_bytes());
    let lock = fixture.data().join(USAGE_LOCK_FILE);
    fs::write(&lock, b"").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        fixture.store().load(),
        Err(UsageStoreError::PermissionsUnsupported)
    );

    fs::remove_file(&lock).unwrap();
    symlink(fixture.ledger(), &lock).unwrap();
    assert_eq!(fixture.store().load(), Err(UsageStoreError::PathUnsafe));
}

#[test]
fn readers_wait_for_a_writer_holding_the_lock() {
    let _no_spawns = hold_off_spawns();
    let fixture = Fixture::new();
    fixture.write([coverage(0), generation(FACT_ID, 1, 1)].concat().as_bytes());
    let holder = PrivateDir::open_existing(&fixture.data()).unwrap().unwrap();
    let held = holder.try_lock(USAGE_LOCK_FILE).unwrap().unwrap();

    let mut store = fixture.store();
    store.lock_deadline = Duration::from_millis(30);
    assert_eq!(store.load(), Err(UsageStoreError::LockBusy));

    let mut store = fixture.store();
    let done = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&done);
    let reader = thread::spawn(move || {
        let loaded = store.load();
        finished.store(true, Ordering::SeqCst);
        loaded
    });
    thread::sleep(Duration::from_millis(100));
    let blocked = !done.load(Ordering::SeqCst);
    drop(held);
    let loaded = reader.join().unwrap().unwrap();
    assert!(blocked);
    assert_eq!(loaded.facts, [fact(FACT_ID, 1, 1)]);
    assert_eq!(mode(&fixture.data().join(USAGE_LOCK_FILE)), 0o600);
}
