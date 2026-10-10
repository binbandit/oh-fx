use super::*;
use crate::profile_usage_store::compaction::COMPACTION_THRESHOLD_BYTES;
use crate::session_log::now_ms;

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
