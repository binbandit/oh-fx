use std::time::Instant;

use super::*;

fn published(fact: &GenerationFact) -> ProfileEvent<'_> {
    ProfileEvent::Generation(fact)
}

fn ledger_text(fixture: &Fixture) -> String {
    fs::read_to_string(fixture.ledger()).unwrap()
}

fn first_line(fixture: &Fixture) -> String {
    let text = ledger_text(fixture);
    format!("{}\n", text.lines().next().unwrap())
}

#[test]
fn the_ledger_is_created_on_the_first_fact_and_an_exact_replay_is_a_duplicate() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    assert!(!fixture.root.path().join("data").exists());
    let first = fact(FACT_ID, 10, 1);
    assert_eq!(
        store.append_event(published(&first)),
        Ok(AppendOutcome::Appended)
    );
    let started = first_line(&fixture);
    assert!(started.starts_with("{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":"));
    assert_eq!(
        ledger_text(&fixture),
        [started.clone(), generation(FACT_ID, 10, 1)].concat()
    );
    assert_eq!(
        store.append_event(published(&first)),
        Ok(AppendOutcome::Duplicate)
    );
    assert_eq!(
        ledger_text(&fixture),
        [started, generation(FACT_ID, 10, 1)].concat()
    );
    assert_eq!(fixture.entries(), [USAGE_FILE, USAGE_LOCK_FILE]);
    assert_eq!(mode(&fixture.data()), 0o700);
    assert_eq!(mode(&fixture.ledger()), 0o600);
    assert_eq!(mode(&fixture.data().join(USAGE_LOCK_FILE)), 0o600);
    assert_eq!(store.load().unwrap().facts, [first]);
}

#[test]
fn pending_markers_incidents_and_conflicting_facts_follow_upstreams_rules() {
    let fixture = Fixture::new();
    fixture.write(coverage(5).as_bytes());
    let mut store = fixture.store();
    let marker = PendingMarker {
        id: FACT_ID.to_owned(),
        observed_at_ms: 7,
    };
    let gap = UsageIncident {
        occurred_at_ms: 8,
        completeness: UsageCompleteness::Incomplete,
    };
    for (event, outcome) in [
        (ProfileEvent::Pending(&marker), AppendOutcome::Appended),
        (ProfileEvent::Pending(&marker), AppendOutcome::Duplicate),
        (ProfileEvent::Incident(&gap), AppendOutcome::Appended),
        (ProfileEvent::Incident(&gap), AppendOutcome::Duplicate),
    ] {
        assert_eq!(store.append_event(event), Ok(outcome), "{event:?}");
    }
    let variants = [
        fact(FACT_ID, 10, 1),
        fact(FACT_ID, 10, 2),
        fact(FACT_ID, 10, 3),
    ];
    let outcomes: Vec<_> = variants
        .iter()
        .map(|variant| store.append_event(published(variant)).unwrap())
        .collect();
    assert_eq!(
        outcomes,
        [
            AppendOutcome::Appended,
            AppendOutcome::Conflict,
            AppendOutcome::Conflict
        ]
    );
    assert_eq!(
        ledger_text(&fixture),
        [
            coverage(5),
            pending(FACT_ID, 7),
            incident(8, "incomplete"),
            generation(FACT_ID, 10, 1),
            generation(FACT_ID, 10, 2),
        ]
        .concat()
    );
}

#[test]
fn invalid_events_are_refused_before_the_ledger_is_touched() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let mut broken = fact(FACT_ID, 10, 1);
    broken.cache_read_tokens = 2;
    let marker = PendingMarker {
        id: "resp_1".to_owned(),
        observed_at_ms: 1,
    };
    let complete = UsageIncident {
        occurred_at_ms: 1,
        completeness: UsageCompleteness::Complete,
    };
    assert_eq!(
        store.append_event(published(&broken)),
        Err(UsageStoreError::InvalidFact)
    );
    assert_eq!(
        store.append_event(ProfileEvent::Pending(&marker)),
        Err(UsageStoreError::InvalidPending)
    );
    assert_eq!(
        store.append_event(ProfileEvent::Incident(&complete)),
        Err(UsageStoreError::InvalidIncident)
    );
    assert!(!fixture.root.path().join("data").exists());
}

#[test]
fn an_incomplete_tail_is_repaired_with_a_recorded_gap() {
    for (appended, outcome) in [
        (fact(OTHER_ID, 11, 1), AppendOutcome::Appended),
        (fact(FACT_ID, 10, 1), AppendOutcome::Duplicate),
    ] {
        let fixture = Fixture::new();
        let complete = [coverage(5), generation(FACT_ID, 10, 1)].concat();
        fixture.write(
            [complete.as_str(), "{\"schema_version\":1,\"kind\""]
                .concat()
                .as_bytes(),
        );
        let mut store = fixture.store();
        assert_eq!(store.append_event(published(&appended)), Ok(outcome));
        let text = ledger_text(&fixture);
        let repaired = text.strip_prefix(complete.as_str()).unwrap();
        let mut lines = repaired.lines();
        let gap = lines.next().unwrap();
        assert!(gap.starts_with("{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":"));
        assert!(gap.ends_with(",\"completeness\":\"incomplete\"}"));
        assert_eq!(
            lines.next().map(|line| format!("{line}\n")),
            (outcome == AppendOutcome::Appended).then(|| generation(OTHER_ID, 11, 1))
        );
        assert_eq!(mode(&fixture.ledger()), 0o600);
        assert_eq!(store.load().unwrap().incidents.len(), 1);
    }
}

#[test]
fn the_ledger_keeps_at_most_4096_incident_records() {
    let fixture = Fixture::new();
    let incidents: String = (0..4096).map(|at| incident(at, "pending")).collect();
    fixture.write([coverage(0), incidents].concat().as_bytes());
    let before = ledger_text(&fixture);
    let mut store = fixture.store();
    let gap = UsageIncident {
        occurred_at_ms: 9_999,
        completeness: UsageCompleteness::Incomplete,
    };
    assert_eq!(
        store.append_event(ProfileEvent::Incident(&gap)),
        Ok(AppendOutcome::Duplicate)
    );
    assert_eq!(ledger_text(&fixture), before);
}

#[test]
fn a_symlinked_ledger_leaf_is_refused() {
    let fixture = Fixture::new();
    fixture.write(coverage(0).as_bytes());
    let target = fixture.root.path().join("elsewhere.jsonl");
    fs::rename(fixture.ledger(), &target).unwrap();
    symlink(&target, fixture.ledger()).unwrap();
    let mut store = fixture.store();
    assert_eq!(
        store.append_event(published(&fact(FACT_ID, 1, 1))),
        Err(UsageStoreError::PathUnsafe)
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), coverage(0));
}

#[test]
fn appends_from_another_writer_are_absorbed_and_a_same_length_replace_is_reparsed() {
    let fixture = Fixture::new();
    let mut ours = fixture.store();
    let mut theirs = fixture.store();
    let first = fact(FACT_ID, 10, 1);
    let second = fact(OTHER_ID, 11, 1);
    ours.append_event(published(&first)).unwrap();
    theirs.append_event(published(&second)).unwrap();
    assert_eq!(
        ours.append_event(published(&second)),
        Ok(AppendOutcome::Duplicate)
    );
    let started = first_line(&fixture);
    let replaced = generation(OTHER_ID, 11, 1).replace("0.25", "0.75");
    fixture.write(
        [started, generation(FACT_ID, 10, 1), replaced]
            .concat()
            .as_bytes(),
    );
    assert_eq!(
        ours.append_event(published(&second)),
        Ok(AppendOutcome::Conflict)
    );
    assert_eq!(ours.load().unwrap().facts.len(), 3);
}

#[test]
fn concurrent_writers_serialize_without_losing_facts() {
    let _no_spawns = hold_off_spawns();
    let fixture = Fixture::new();
    let data = fixture.data();
    let ids: Vec<String> = "ABCDEFGHJKMNPQRS"
        .chars()
        .map(|suffix| format!("gen_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}"))
        .collect();
    let writers: Vec<_> = ids
        .chunks(4)
        .map(|chunk| {
            let data = data.clone();
            let chunk = chunk.to_vec();
            thread::spawn(move || {
                let mut store = ProfileUsageStore::open(&data).unwrap();
                for id in chunk {
                    store.append_event(published(&fact(&id, 10, 1))).unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let loaded = fixture.store().load().unwrap();
    assert_eq!(loaded.facts.len(), ids.len());
    assert_eq!(ledger_text(&fixture).lines().count(), ids.len() + 1);
}

#[test]
fn a_busy_lock_times_out_and_abandoning_ends_the_wait() {
    let _no_spawns = hold_off_spawns();
    let fixture = Fixture::new();
    fixture.write(coverage(0).as_bytes());
    let holder = PrivateDir::open_existing(&fixture.data()).unwrap().unwrap();
    let held = holder.try_lock(USAGE_LOCK_FILE).unwrap().unwrap();
    let first = fact(FACT_ID, 1, 1);

    let mut store = fixture.store();
    store.lock_deadline = Duration::from_millis(30);
    assert_eq!(
        store.append_event(published(&first)),
        Err(UsageStoreError::LockBusy)
    );

    let mut store = fixture.store();
    let abandon = store.abandon_flag();
    let started = Instant::now();
    let waiter = thread::spawn(move || {
        let outcome = store.append_event(published(&fact(FACT_ID, 1, 1)));
        (outcome, store)
    });
    thread::sleep(Duration::from_millis(100));
    abandon.store(true, Ordering::Release);
    let (outcome, mut store) = waiter.join().unwrap();
    assert_eq!(outcome, Err(UsageStoreError::LockAbandoned));
    assert!(started.elapsed() < LOCK_DEADLINE);
    drop(held);
    assert_eq!(
        store.append_event(published(&first)),
        Err(UsageStoreError::LockAbandoned)
    );
    assert_eq!(ledger_text(&fixture), coverage(0));
}
