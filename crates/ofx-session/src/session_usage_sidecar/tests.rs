use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;
use crate::session_usage::Availability;

const SESSION_ID: &str = "AbCdEfGhIjKl";
const FRESH: &str = "{\"schema_version\":1,\"session_id\":\"AbCdEfGhIjKl\",\"snapshot\":{\"schema_version\":3,\"billing\":\"complete\",\"api_duration_complete\":true,\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":1,\"settled_through_sequence\":0,\"api_duration_ms\":0,\"wall_duration_ms\":0,\"total_cost\":0,\"input_tokens\":0,\"output_tokens\":0,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"reasoning_tokens\":0,\"request_count\":0,\"billable_web_search_calls\":0,\"lines_added\":0,\"lines_removed\":0,\"models\":[],\"pending\":[],\"publication_backlog\":[],\"incidents\":[]}}";

struct Fixture {
    root: tempfile::TempDir,
    dir: PrivateDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open_or_create(&root.path().join("session")).unwrap();
        Self { root, dir }
    }

    fn sidecar(&self) -> PathBuf {
        self.root.path().join("session").join(SIDECAR_FILE)
    }

    fn put(&self, bytes: &[u8]) {
        fs::write(self.sidecar(), bytes).unwrap();
        fs::set_permissions(self.sidecar(), fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn load(&self) -> Result<UsageSnapshot, SessionError> {
        load_conversation(&self.dir, SESSION_ID, 10)
    }
}

fn gap(occurred_at_ms: i64) -> UsageSnapshot {
    let mut snapshot = UsageSnapshot::unavailable();
    snapshot.incidents.push(UsageIncident {
        occurred_at_ms,
        completeness: UsageCompleteness::Incomplete,
    });
    snapshot
}

#[test]
fn a_fresh_sidecar_is_written_privately_in_upstreams_bytes_and_reads_back() {
    let fixture = Fixture::new();
    write(&fixture.dir, SESSION_ID, &UsageSnapshot::fresh()).unwrap();
    assert_eq!(fs::read_to_string(fixture.sidecar()).unwrap(), FRESH);
    assert_eq!(
        fs::metadata(fixture.sidecar())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(fixture.load().unwrap(), UsageSnapshot::fresh());
}

#[test]
fn a_missing_or_damaged_sidecar_loads_as_incomplete_with_one_gap() {
    let fixture = Fixture::new();
    assert_eq!(fixture.load().unwrap(), gap(10));
    assert_eq!(
        load_conversation(&fixture.dir, SESSION_ID, -5).unwrap(),
        gap(0)
    );
    assert!(!fixture.sidecar().exists());

    for damaged in [
        b"".to_vec(),
        b"{broken".to_vec(),
        vec![b' '; MAX_SIDECAR_BYTES + 1],
        FRESH.replace("AbCdEfGhIjKl", "SomeoneElse0").into_bytes(),
        FRESH
            .replace("\"session_id\":\"AbCdEfGhIjKl\"", "\"session_id\":\"\"")
            .into_bytes(),
        FRESH
            .replacen("\"schema_version\":1", "\"schema_version\":2", 1)
            .into_bytes(),
        FRESH
            .replacen("\"schema_version\":1", "\"schema_version\":1.0", 1)
            .into_bytes(),
        FRESH.replacen("\"schema_version\":3,", "", 1).into_bytes(),
        FRESH
            .replacen("\"next_sequence\":1", "\"next_sequence\":0", 1)
            .into_bytes(),
        FRESH.replacen('}', ",\"extra\":1}", 1).into_bytes(),
    ] {
        fixture.put(&damaged);
        assert_eq!(
            fixture.load().unwrap(),
            gap(10),
            "{}",
            String::from_utf8_lossy(&damaged[..damaged.len().min(80)])
        );
        assert_eq!(fs::read(fixture.sidecar()).unwrap(), damaged);
    }
}

#[test]
fn unsafe_sidecars_refuse_the_load() {
    let fixture = Fixture::new();
    fixture.put(FRESH.as_bytes());
    fs::set_permissions(fixture.sidecar(), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(fixture.load(), Err(SessionError::InvalidUsageSidecar));

    fs::set_permissions(fixture.sidecar(), fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(fixture.sidecar(), fixture.root.path().join("linked")).unwrap();
    assert_eq!(fixture.load(), Err(SessionError::InvalidUsageSidecar));
    fs::remove_file(fixture.root.path().join("linked")).unwrap();
    assert!(fixture.load().is_ok());

    fs::rename(fixture.sidecar(), fixture.root.path().join("target")).unwrap();
    symlink(fixture.root.path().join("target"), fixture.sidecar()).unwrap();
    assert_eq!(fixture.load(), Err(SessionError::InvalidUsageSidecar));

    fs::remove_file(fixture.sidecar()).unwrap();
    fs::create_dir(fixture.sidecar()).unwrap();
    assert_eq!(fixture.load(), Err(SessionError::InvalidUsageSidecar));
}

#[test]
fn a_sidecar_fx_wrote_is_restored_as_written() {
    let fixture = Fixture::new();
    let fx = FRESH
        .replace("\"billing\":\"complete\"", "\"billing\":\"incomplete\"")
        .replace(
            "\"next_sequence\":1,\"settled_through_sequence\":0",
            "\"next_sequence\":3,\"settled_through_sequence\":2",
        )
        .replace(
            "\"lines_added\":0,\"lines_removed\":0",
            "\"lines_added\":12,\"lines_removed\":4",
        )
        .replace(
            "\"incidents\":[]",
            "\"incidents\":[{\"occurred_at_ms\":5,\"completeness\":\"incomplete\"}]",
        );
    fixture.put(fx.as_bytes());
    let snapshot = fixture.load().unwrap();
    assert_eq!(snapshot.billing, Availability::Incomplete);
    assert_eq!(snapshot.next_sequence, 3);
    assert_eq!(snapshot.lines_added, 12);
    assert_eq!(snapshot.incidents.len(), 1);
    write(&fixture.dir, SESSION_ID, &snapshot).unwrap();
    assert_eq!(fs::read_to_string(fixture.sidecar()).unwrap(), fx);
}

#[test]
fn oversized_snapshots_are_not_written() {
    let fixture = Fixture::new();
    let mut snapshot = UsageSnapshot::fresh();
    snapshot.billing = Availability::Incomplete;
    for index in 0..16_i64 {
        snapshot
            .append_incident(UsageIncident {
                occurred_at_ms: index,
                completeness: UsageCompleteness::Incomplete,
            })
            .unwrap();
    }
    assert!(encode(&"s".repeat(MAX_SIDECAR_BYTES), &snapshot).is_err());
    assert_eq!(
        write(&fixture.dir, &"s".repeat(MAX_SIDECAR_BYTES), &snapshot),
        Err(SessionError::UsageSidecarTooLarge)
    );
    assert!(!fixture.sidecar().exists());
}

#[test]
fn recovery_repairs_damaged_accounting_and_refuses_foreign_or_unsupported_records() {
    let fixture = Fixture::new();
    let classify = || has_recoverable_corruption(&fixture.dir, SESSION_ID);
    assert_eq!(classify(), Ok(false));
    fixture.put(FRESH.as_bytes());
    assert_eq!(classify(), Ok(false));
    for repairable in [&b""[..], b"not json", b"[]", b"{\"schema_version\":1}"] {
        fixture.put(repairable);
        assert_eq!(
            classify(),
            Ok(true),
            "{}",
            String::from_utf8_lossy(repairable)
        );
    }
    for (record, error) in [
        (
            FRESH.replace("AbCdEfGhIjKl", "SomeoneElse0"),
            SessionError::UsageSidecarSessionMismatch,
        ),
        (
            FRESH.replace("{\"schema_version\":1,", "{\"schema_version\":2,"),
            SessionError::UnsupportedUsageSidecar,
        ),
        (
            FRESH.replace(
                "\"snapshot\":{\"schema_version\":3,",
                "\"snapshot\":{\"schema_version\":9,",
            ),
            SessionError::UnsupportedUsageSidecar,
        ),
        (
            FRESH.replace("{\"schema_version\":1,", "{\"schema_version\":-1,"),
            SessionError::UnsupportedUsageSidecar,
        ),
    ] {
        fixture.put(record.as_bytes());
        assert_eq!(classify(), Err(error), "{record}");
    }
    fixture.put(FRESH.as_bytes());
    fs::set_permissions(fixture.sidecar(), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(classify(), Err(SessionError::InvalidUsageSidecar));
}
