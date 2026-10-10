use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ofx_contract::{GenerationFact, UsageCoverage};

use super::*;

const NOW: i64 = 1_775_045_467_000;
const FACT: &str = "{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"created_at_ms\":1775045466999,\"model\":\"provider/model\",\"input_tokens\":5,\"output_tokens\":2,\"cache_read_tokens\":1,\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"billable_web_search_calls\":1,\"total_cost\":0.25}}\n";
const PENDING: &str = "{\"schema_version\":1,\"kind\":\"pending\",\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"observed_at_ms\":1775045466999}\n";
const INCIDENT: &str = "{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":1775045467000,\"completeness\":\"incomplete\"}\n";

struct Profile {
    root: tempfile::TempDir,
}

impl Profile {
    fn with_ledger(records: &[&str]) -> Self {
        let profile = Self {
            root: tempfile::tempdir().unwrap(),
        };
        let data = profile.data();
        fs::create_dir_all(&data).unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        let ledger = data.join("usage.jsonl");
        let coverage = format!(
            "{{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":{}}}\n",
            NOW - 2
        );
        fs::write(
            &ledger,
            [coverage.as_str()]
                .iter()
                .chain(records)
                .copied()
                .collect::<String>(),
        )
        .unwrap();
        fs::set_permissions(&ledger, fs::Permissions::from_mode(0o600)).unwrap();
        profile
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("oh-fx")
    }

    fn report(&self, scope: UsageScope, snapshot_time_ms: i64) -> UsageReport {
        ProfileUsage::open(&self.data())
            .unwrap()
            .report(scope, snapshot_time_ms)
            .unwrap()
    }
}

#[test]
fn reports_cover_only_durable_profile_facts() {
    let profile = Profile::with_ledger(&[FACT]);
    for scope in [UsageScope::Days30, UsageScope::Days7, UsageScope::Hours24] {
        let report = profile.report(scope, NOW + 1);
        assert_eq!(report.scope, scope);
        assert_eq!(report.snapshot_time_ms, NOW + 1);
        assert_eq!(report.coverage, UsageCoverage::Partial);
        assert_eq!(report.coverage_started_at_ms, Some(NOW - 2));
        assert_eq!(report.completeness, UsageCompleteness::Complete);
        let totals = report.totals.unwrap();
        assert_eq!(totals.total_tokens, 7);
        assert_eq!(totals.request_count, Some(1));
        assert_eq!(totals.reasoning_tokens, None);
    }
}

#[test]
fn durable_pending_markers_stay_pending_until_their_fact_lands() {
    let pending = Profile::with_ledger(&[PENDING]).report(UsageScope::Hours24, NOW + 1);
    assert_eq!(pending.completeness, UsageCompleteness::Pending);
    assert_eq!(pending.totals.unwrap().total_tokens, 0);

    let settled = Profile::with_ledger(&[PENDING, FACT]).report(UsageScope::Hours24, NOW + 1);
    assert_eq!(settled.completeness, UsageCompleteness::Complete);
    assert_eq!(settled.totals.unwrap().total_tokens, 7);

    let incomplete = Profile::with_ledger(&[PENDING, FACT, INCIDENT, INCIDENT])
        .report(UsageScope::Hours24, NOW + 1);
    assert_eq!(incomplete.completeness, UsageCompleteness::Incomplete);
    assert_eq!(incomplete.totals.unwrap().total_tokens, 7);
}

#[test]
fn a_pending_marker_from_the_future_marks_the_report_incomplete_just_before_now() {
    let report = Profile::with_ledger(&[PENDING]).report(UsageScope::Hours24, NOW - 1);
    assert_eq!(report.completeness, UsageCompleteness::Incomplete);
    let just_after = Profile::with_ledger(&[PENDING]).report(UsageScope::Hours24, NOW);
    assert_eq!(just_after.completeness, UsageCompleteness::Pending);
}

#[test]
fn an_empty_profile_has_not_started_tracking() {
    let root = tempfile::tempdir().unwrap();
    let report = ProfileUsage::open(&root.path().join("missing/oh-fx"))
        .unwrap()
        .report(UsageScope::Days30, NOW)
        .unwrap();
    assert_eq!(report.coverage, UsageCoverage::NotStarted);
    assert_eq!(report.completeness, UsageCompleteness::Complete);
    assert_eq!(report.totals, None);
    assert!(!root.path().join("missing").exists());
}

#[test]
fn errors_keep_upstream_names() {
    let profile = Profile::with_ledger(&[]);
    fs::set_permissions(profile.data(), fs::Permissions::from_mode(0o750)).unwrap();
    let error = ProfileUsage::open(&profile.data())
        .unwrap()
        .report(UsageScope::Days30, NOW)
        .unwrap_err();
    assert_eq!(error.to_string(), "PrivateStatePermissionsUnsupported");
    assert_eq!(
        ProfileUsageError::from(UsageReportError::InvalidSnapshotTime).to_string(),
        "InvalidSnapshotTime"
    );
}

#[test]
fn publishing_appends_once_reports_conflicts_and_stops_when_abandoned() {
    let profile = Profile::with_ledger(&[FACT]);
    let publisher = ProfilePublisher::open(&profile.data()).unwrap();
    let recorded = GenerationFact {
        id: "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
        created_at_ms: NOW - 1,
        model: "provider/model".to_owned(),
        input_tokens: 5,
        output_tokens: 2,
        cache_read_tokens: 1,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        billable_web_search_calls: 1,
        total_cost: 0.25,
    };
    assert_eq!(
        publisher.publish(ProfileEvent::Generation(&recorded)),
        Ok(())
    );
    let conflicting = GenerationFact {
        input_tokens: 6,
        ..recorded.clone()
    };
    let conflict = publisher.publish(ProfileEvent::Generation(&conflicting));
    assert_eq!(conflict, Err(ProfileUsageError::Conflict));
    assert!(!conflict.unwrap_err().ledger_unavailable());

    publisher.abandon_for_process_exit();
    let abandoned = publisher
        .clone()
        .publish(ProfileEvent::Generation(&recorded))
        .unwrap_err();
    assert_eq!(
        abandoned,
        ProfileUsageError::Store(UsageStoreError::LockAbandoned)
    );
    assert!(abandoned.ledger_unavailable());
}
