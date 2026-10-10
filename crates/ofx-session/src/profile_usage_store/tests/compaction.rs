use ofx_contract::UsageScope;

use super::*;
use crate::profile_usage_runtime::ProfileUsage;
use crate::profile_usage_store::compaction::COMPACTION_THRESHOLD_BYTES;
use crate::session_log::now_ms;
use crate::usage_recovery::UsageRecovery;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const NOW: i64 = 1_775_045_467_000;

fn indexed(lines: &[String]) -> RecordIndex {
    let mut index = RecordIndex::default();
    index.absorb_bytes(lines.concat().as_bytes(), None).unwrap();
    index
}

fn appended(
    store: &mut ProfileUsageStore,
    fact: &GenerationFact,
) -> Result<AppendOutcome, UsageStoreError> {
    store.append_event(ProfileEvent::Generation(fact))
}

#[test]
fn compaction_needs_expired_records_and_waits_a_day_of_slack_after_an_append() {
    let recent = NOW - 34 * DAY_MS;
    let expired = NOW - 35 * DAY_MS - 1;
    let aged = NOW - 36 * DAY_MS - 1;
    let fresh = indexed(&[
        coverage(0),
        generation(FACT_ID, recent, 1),
        pending(OTHER_ID, recent),
        incident(recent, "incomplete"),
    ]);
    assert!(!fresh.has_expired(NOW));
    assert!(!fresh.has_aged(NOW));
    for (stale, ages) in [
        (generation(FACT_ID, expired, 1), false),
        (generation(FACT_ID, aged, 1), true),
        (pending(OTHER_ID, expired), false),
        (pending(OTHER_ID, aged), true),
        (incident(expired, "pending"), false),
        (incident(aged, "pending"), true),
    ] {
        let index = indexed(&[coverage(0), stale.clone()]);
        assert!(index.has_expired(NOW), "{stale}");
        assert_eq!(index.has_aged(NOW), ages, "{stale}");
    }
    let resolved = indexed(&[
        coverage(0),
        pending(FACT_ID, recent),
        generation(FACT_ID, recent, 1),
    ]);
    assert!(resolved.has_expired(NOW));
    assert!(!resolved.has_aged(NOW));
}

#[test]
fn compaction_keeps_coverage_and_every_record_inside_the_retention_window() {
    let recent = NOW - DAY_MS;
    let expired = NOW - 40 * DAY_MS;
    let index = indexed(&[
        coverage(5),
        incident(recent, "incomplete"),
        generation(FACT_ID, expired, 1),
        pending(FACT_ID, recent),
        generation(OTHER_ID, recent, 1),
        generation(OTHER_ID, recent, 2),
        pending("gen_01ARZ3NDEKTSV4RRFFQ69G5FAX", recent),
        pending("gen_01ARZ3NDEKTSV4RRFFQ69G5FAY", expired),
        incident(expired, "pending"),
    ]);
    let kept = [
        coverage(5),
        generation(OTHER_ID, recent, 1),
        generation(OTHER_ID, recent, 2),
        pending("gen_01ARZ3NDEKTSV4RRFFQ69G5FAX", recent),
        incident(recent, "incomplete"),
    ];
    assert_eq!(index.retained_lines(NOW), kept.concat());
    assert_eq!(index.retained_count(NOW), kept.len());
    assert_eq!(indexed(&[]).retained_lines(NOW), "");
}

#[test]
fn a_full_ledger_compacts_expired_records_before_the_append_or_refuses_it() {
    let now = now_ms();
    let first = fact(FACT_ID, now, 1);
    for (occurred_at_ms, compacts) in [(now - 40 * DAY_MS, true), (now - DAY_MS, false)] {
        let fixture = Fixture::new();
        let started = coverage(now - 50 * DAY_MS);
        let filler = incident(occurred_at_ms, "incomplete").repeat(MAX_RECORDS - 1);
        fixture.write([started.as_str(), filler.as_str()].concat().as_bytes());
        let mut store = fixture.store();
        let outcome = appended(&mut store, &first);
        let ledger = fs::read_to_string(fixture.ledger()).unwrap();
        if compacts {
            assert_eq!(outcome, Ok(AppendOutcome::Appended));
            assert_eq!(ledger, [started, generation(FACT_ID, now, 1)].concat());
            assert_eq!(store.load().unwrap().facts, std::slice::from_ref(&first));
        } else {
            assert_eq!(outcome, Err(UsageStoreError::CapacityExceeded));
            assert_eq!(ledger.len(), started.len() + filler.len());
        }
    }
}

#[test]
fn a_full_ledger_with_a_torn_tail_is_compacted_and_repaired_in_one_replace() {
    let now = now_ms();
    let started = coverage(now - 50 * DAY_MS);
    let filler = incident(now - 40 * DAY_MS, "incomplete").repeat(MAX_RECORDS - 1);
    let fixture = Fixture::new();
    fixture.write(
        [started.as_str(), filler.as_str(), "{\"schema_version\":1"]
            .concat()
            .as_bytes(),
    );
    let mut store = fixture.store();
    assert_eq!(
        appended(&mut store, &fact(FACT_ID, now, 1)),
        Ok(AppendOutcome::Appended)
    );
    let ledger = fs::read_to_string(fixture.ledger()).unwrap();
    let lines: Vec<&str> = ledger
        .strip_prefix(started.as_str())
        .unwrap()
        .lines()
        .collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("{\"schema_version\":1,\"kind\":\"incident\","));
    assert!(lines[0].ends_with("\"completeness\":\"incomplete\"}"));
    assert_eq!(format!("{}\n", lines[1]), generation(FACT_ID, now, 1));
}

#[test]
fn a_ledger_past_eight_mib_compacts_after_an_append_once_records_have_aged() {
    let now = now_ms();
    let line = incident(now - 40 * DAY_MS, "incomplete");
    let filler = line.repeat(usize::try_from(COMPACTION_THRESHOLD_BYTES).unwrap() / line.len() + 1);
    let started = coverage(now - 50 * DAY_MS);
    let fixture = Fixture::new();
    fixture.write([started.as_str(), filler.as_str()].concat().as_bytes());
    let mut store = fixture.store();
    let first = fact(FACT_ID, now, 1);
    assert_eq!(appended(&mut store, &first), Ok(AppendOutcome::Appended));
    assert_eq!(
        fs::read_to_string(fixture.ledger()).unwrap(),
        [started.clone(), generation(FACT_ID, now, 1)].concat()
    );
    assert_eq!(appended(&mut store, &first), Ok(AppendOutcome::Duplicate));

    let recent = incident(now - DAY_MS, "incomplete");
    let filler =
        recent.repeat(usize::try_from(COMPACTION_THRESHOLD_BYTES).unwrap() / recent.len() + 1);
    let fixture = Fixture::new();
    fixture.write([started.as_str(), filler.as_str()].concat().as_bytes());
    let mut store = fixture.store();
    assert_eq!(appended(&mut store, &first), Ok(AppendOutcome::Appended));
    assert_eq!(
        fs::read_to_string(fixture.ledger()).unwrap(),
        [started, filler, generation(FACT_ID, now, 1)].concat()
    );
}

fn filler_id(index: usize) -> String {
    format!("gen_{index:026}")
}

fn tail_of(ledger: &str) -> &str {
    &ledger[ledger.len() - 32..]
}

#[test]
fn a_peer_never_compacts_from_records_another_writer_has_replaced() {
    let now = now_ms();
    let expired = now - 40 * DAY_MS;
    let mut ledger = coverage(now - 50 * DAY_MS);
    for index in 0..MAX_RECORDS - 3 {
        ledger.push_str(&pending(&filler_id(index), now - DAY_MS));
    }
    ledger.push_str(&incident(expired, "incomplete"));
    ledger.push_str(&incident(now, "incomplete"));
    let fixture = Fixture::new();
    fixture.write(ledger.as_bytes());
    let mut first = fixture.store();
    let mut second = fixture.store();
    let cached = PendingMarker {
        id: filler_id(0),
        observed_at_ms: now - DAY_MS,
    };
    assert_eq!(
        second.append_event(ProfileEvent::Pending(&cached)),
        Ok(AppendOutcome::Duplicate)
    );

    let acknowledged = UsageIncident {
        occurred_at_ms: now - 1_000,
        completeness: UsageCompleteness::Incomplete,
    };
    assert_eq!(
        first.append_event(ProfileEvent::Incident(&acknowledged)),
        Ok(AppendOutcome::Appended)
    );
    let replaced = fs::read_to_string(fixture.ledger()).unwrap();
    assert_eq!(replaced.len(), ledger.len());
    assert_eq!(tail_of(&replaced), tail_of(&ledger));

    assert_eq!(
        second.append_event(ProfileEvent::Generation(&fact(FACT_ID, now, 1))),
        Err(UsageStoreError::CapacityExceeded)
    );
    assert_eq!(fs::read_to_string(fixture.ledger()).unwrap(), replaced);
    let loaded = fixture.store().load().unwrap();
    assert!(loaded.incidents.contains(&acknowledged));
    assert!(
        loaded
            .incidents
            .iter()
            .all(|incident| incident.occurred_at_ms != expired)
    );
}

#[test]
fn a_replacement_with_the_same_length_and_tail_is_read_again() {
    let now = now_ms();
    let fixture = Fixture::new();
    let last = incident(now, "incomplete");
    fixture.write(
        [coverage(0), generation(FACT_ID, now, 1), last.clone()]
            .concat()
            .as_bytes(),
    );
    let mut store = fixture.store();
    assert_eq!(
        store.append_event(ProfileEvent::Generation(&fact(FACT_ID, now, 1))),
        Ok(AppendOutcome::Duplicate)
    );
    let swapped = [coverage(0), generation(OTHER_ID, now, 1), last].concat();
    let staged = fixture.data().join("staged");
    fs::write(&staged, &swapped).unwrap();
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&staged, fixture.ledger()).unwrap();

    assert_eq!(
        store.append_event(ProfileEvent::Generation(&fact(OTHER_ID, now, 1))),
        Ok(AppendOutcome::Duplicate)
    );
    assert_eq!(fs::read_to_string(fixture.ledger()).unwrap(), swapped);
}

#[test]
fn compaction_keeps_conflicting_pending_markers_without_writing_their_derived_incidents() {
    let now = NOW;
    let index = indexed(&[
        coverage(0),
        pending(FACT_ID, now - DAY_MS),
        pending(FACT_ID, now - 2 * DAY_MS),
        incident(now - DAY_MS, "pending"),
    ]);
    assert_eq!(index.loaded.incidents.len(), 2);
    let kept = [
        coverage(0),
        pending(FACT_ID, now - DAY_MS),
        pending(FACT_ID, now - 2 * DAY_MS),
        incident(now - DAY_MS, "pending"),
    ];
    assert_eq!(index.retained_lines(now), kept.concat());
    assert_eq!(index.retained_count(now), kept.len());
    let reread = indexed(&kept);
    assert_eq!(reread.loaded.incidents, index.loaded.incidents);
}

#[test]
fn compacting_a_ledger_of_conflicting_pending_markers_stays_within_the_record_limit() {
    let now = now_ms();
    let mut ledger = coverage(now - 50 * DAY_MS);
    for index in 0..(MAX_RECORDS - 3) / 2 {
        ledger.push_str(&pending(&filler_id(index), now - DAY_MS));
        ledger.push_str(&pending(&filler_id(index), now - DAY_MS - 1));
    }
    let rows = ledger.lines().count();
    let fixture = Fixture::new();
    fixture.write(ledger.as_bytes());
    let mut store = fixture.store();
    let old = fact(FACT_ID, now - 40 * DAY_MS, 1);
    assert_eq!(
        store.append_event(ProfileEvent::Generation(&old)),
        Ok(AppendOutcome::Appended)
    );
    let compacted = fs::read_to_string(fixture.ledger()).unwrap();
    assert_eq!(compacted, ledger);
    assert_eq!(compacted.lines().count(), rows);
    let loaded = fixture.store().load().unwrap();
    assert_eq!(loaded.pending.len(), rows - 1);
    assert_eq!(loaded.incidents.len(), (rows - 1) / 2);
}

fn completeness_of(ledger: &str) -> UsageCompleteness {
    let fixture = Fixture::new();
    fixture.write(ledger.as_bytes());
    ProfileUsage::open(&fixture.data())
        .unwrap()
        .report(UsageScope::Days30, NOW, &UsageRecovery::default())
        .unwrap()
        .completeness
}

#[test]
fn a_conflict_incident_is_written_unless_both_of_its_markers_survive() {
    let recent = NOW - DAY_MS;
    let earlier = NOW - 2 * DAY_MS;
    let expired = NOW - 40 * DAY_MS;
    let cases = [
        (
            "both markers survive",
            vec![
                coverage(0),
                pending(FACT_ID, recent),
                pending(FACT_ID, earlier),
            ],
            vec![
                coverage(0),
                pending(FACT_ID, recent),
                pending(FACT_ID, earlier),
            ],
            UsageCompleteness::Incomplete,
        ),
        (
            "a fact resolves the conflict",
            vec![
                coverage(0),
                pending(FACT_ID, earlier),
                pending(FACT_ID, recent),
                generation(FACT_ID, recent, 1),
            ],
            vec![
                coverage(0),
                generation(FACT_ID, recent, 1),
                incident(recent, "incomplete"),
            ],
            UsageCompleteness::Incomplete,
        ),
        (
            "the first marker expired",
            vec![
                coverage(0),
                pending(FACT_ID, expired),
                pending(FACT_ID, recent),
            ],
            vec![
                coverage(0),
                pending(FACT_ID, recent),
                incident(recent, "incomplete"),
            ],
            UsageCompleteness::Incomplete,
        ),
        (
            "the conflicting marker expired with its incident",
            vec![
                coverage(0),
                pending(FACT_ID, recent),
                pending(FACT_ID, expired),
            ],
            vec![coverage(0), pending(FACT_ID, recent)],
            UsageCompleteness::Pending,
        ),
        (
            "both markers expired with their incident",
            vec![
                coverage(0),
                pending(FACT_ID, expired),
                pending(FACT_ID, expired - DAY_MS),
            ],
            vec![coverage(0)],
            UsageCompleteness::Complete,
        ),
    ];
    for (case, ledger, kept, completeness) in cases {
        let index = indexed(&ledger);
        let compacted = index.retained_lines(NOW);
        assert_eq!(compacted, kept.concat(), "{case}");
        assert_eq!(index.retained_count(NOW), kept.len(), "{case}");
        assert_eq!(completeness_of(&ledger.concat()), completeness, "{case}");
        assert_eq!(completeness_of(&compacted), completeness, "{case}");
    }
}
