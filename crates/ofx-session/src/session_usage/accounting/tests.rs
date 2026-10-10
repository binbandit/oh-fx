use super::*;

const NOW: i64 = 1_775_045_467_000;

fn lines(additions: u32, deletions: u32) -> FileChangeStats {
    FileChangeStats {
        additions,
        deletions,
    }
}

#[test]
fn an_active_request_never_reads_as_complete() {
    let mut usage = Usage::fresh();
    let sequence = usage.reserve(NOW).unwrap();
    assert_eq!(sequence, 1);
    let active = usage.snapshot(NOW);
    assert_eq!(active.billing, Availability::Incomplete);
    assert!(!active.api_duration_complete);
    assert_eq!(active.next_sequence, 2);
    assert_eq!(active.settled_through_sequence, 0);

    assert!(usage.finish(sequence, 10, DeliveryOutcome::Unbilled, NOW));
    let finished = usage.snapshot(NOW);
    assert_eq!(finished.billing, Availability::Complete);
    assert!(finished.api_duration_complete);
    assert_eq!(finished.api_duration_ms, 10);
    assert_eq!(finished.settled_through_sequence, 1);
    assert!(finished.incidents.is_empty());
    assert_eq!(finished.validate(), Ok(()));
}

#[test]
fn requests_settle_only_when_every_active_one_finishes() {
    let mut usage = Usage::fresh();
    let first = usage.reserve(NOW).unwrap();
    let second = usage.reserve(NOW).unwrap();
    assert!(usage.finish(second, 3, DeliveryOutcome::Unbilled, NOW));
    assert_eq!(usage.snapshot(NOW).settled_through_sequence, 0);
    assert!(!usage.finish(second, 3, DeliveryOutcome::Unbilled, NOW));
    assert!(usage.finish(first, 4, DeliveryOutcome::Unbilled, NOW));
    let settled = usage.snapshot(NOW);
    assert_eq!(settled.settled_through_sequence, 2);
    assert_eq!(settled.api_duration_ms, 7);
    assert_eq!(settled.billing, Availability::Complete);
}

#[test]
fn billed_requests_without_exact_usage_leave_billing_incomplete_with_one_gap() {
    for outcome in [
        DeliveryOutcome::PossiblyBilledWithoutIdentity,
        DeliveryOutcome::AmbiguousDelivery,
    ] {
        let mut usage = Usage::fresh();
        let first = usage.reserve(NOW).unwrap();
        assert!(usage.finish(first, 1, outcome, NOW));
        let second = usage.reserve(NOW).unwrap();
        assert!(usage.finish(second, 1, outcome, NOW));
        let snapshot = usage.snapshot(NOW);
        assert_eq!(snapshot.billing, Availability::Incomplete, "{outcome:?}");
        assert_eq!(
            snapshot.incidents,
            [UsageIncident {
                occurred_at_ms: NOW,
                completeness: UsageCompleteness::Incomplete,
            }]
        );
        assert_eq!(snapshot.validate(), Ok(()));
    }
}

#[test]
fn unknown_sequences_mark_the_api_time_incomplete() {
    let mut usage = Usage::fresh();
    assert!(!usage.finish(1, 5, DeliveryOutcome::Unbilled, NOW));
    let snapshot = usage.snapshot(NOW);
    assert_eq!(snapshot.billing, Availability::Incomplete);
    assert!(!snapshot.api_duration_complete);
    assert_eq!(snapshot.api_duration_ms, 0);

    let mut usage = Usage::fresh();
    let sequence = usage.reserve(NOW).unwrap();
    assert!(!usage.finish(0, 5, DeliveryOutcome::Unbilled, NOW));
    assert!(usage.finish(sequence, u64::MAX, DeliveryOutcome::Unbilled, NOW));
    let second = usage.reserve(NOW).unwrap();
    assert!(!usage.finish(second, 1, DeliveryOutcome::Unbilled, NOW));
    assert!(!usage.snapshot(NOW).api_duration_complete);
}

#[test]
fn active_request_capacity_fails_before_admission_and_frees_on_finish() {
    let mut usage = Usage::fresh();
    let sequences: Vec<u64> = (0..MAX_ACTIVE_INVOCATIONS)
        .map(|_| usage.reserve(NOW).unwrap())
        .collect();
    assert_eq!(usage.reserve(NOW), Err(ReserveFailure::CapacityExceeded));
    assert!(usage.finish(sequences[0], 0, DeliveryOutcome::Unbilled, NOW));
    let replacement = usage.reserve(NOW).unwrap();
    for sequence in sequences[1..].iter().chain([&replacement]) {
        assert!(usage.finish(*sequence, 0, DeliveryOutcome::Unbilled, NOW));
    }
    let snapshot = usage.snapshot(NOW);
    assert_eq!(snapshot.billing, Availability::Complete);
    assert!(snapshot.api_duration_complete);
    assert_eq!(snapshot.next_sequence, 66);
    assert_eq!(snapshot.settled_through_sequence, 65);
}

#[test]
fn wall_time_starts_at_the_first_request_or_the_saved_session_start() {
    let mut fresh = Usage::fresh();
    assert_eq!(fresh.snapshot(NOW).wall_duration_ms, 0);
    assert_eq!(fresh.snapshot(NOW + 400).wall_duration_ms, 400);

    let mut lazy = Usage::fresh();
    lazy.reserve(NOW + 100).unwrap();
    assert_eq!(lazy.snapshot(NOW + 150).wall_duration_ms, 50);

    let mut saved = UsageSnapshot::fresh();
    saved.wall_duration_ms = 9_999;
    let mut resumed = Usage::restore(saved.clone(), NOW - 500, NOW);
    let current = resumed.snapshot(NOW);
    assert!(current.wall_duration_complete);
    assert_eq!(current.wall_duration_ms, 500);

    for started in [0, NOW + 1] {
        let mut unknown = Usage::restore(saved.clone(), started, NOW);
        let current = unknown.snapshot(NOW + 20);
        assert!(!current.wall_duration_complete);
        assert_eq!(current.wall_duration_ms, 20);
    }
}

#[test]
fn committed_lines_add_up_and_overflow_marks_code_incomplete() {
    let mut usage = Usage::fresh();
    usage.record_committed_lines(lines(3, 1));
    usage.record_committed_lines(lines(2, 4));
    let snapshot = usage.snapshot(NOW);
    assert_eq!((snapshot.lines_added, snapshot.lines_removed), (5, 5));
    assert!(snapshot.code_complete);

    let mut saved = UsageSnapshot::fresh();
    saved.lines_added = u64::MAX;
    let mut full = Usage::restore(saved.clone(), NOW, NOW);
    full.record_committed_lines(lines(1, 1));
    let snapshot = full.snapshot(NOW);
    assert_eq!(
        (snapshot.lines_added, snapshot.lines_removed),
        (u64::MAX, 0)
    );
    assert!(!snapshot.code_complete);

    saved.lines_added = 0;
    saved.lines_removed = u64::MAX;
    let mut removed = Usage::restore(saved, NOW, NOW);
    removed.record_committed_lines(lines(2, 1));
    let snapshot = removed.snapshot(NOW);
    assert_eq!(
        (snapshot.lines_added, snapshot.lines_removed),
        (2, u64::MAX)
    );
    assert!(!snapshot.code_complete);
}

#[test]
fn incidents_keep_upstreams_sixteen_record_limit() {
    let mut usage = Usage::fresh();
    for offset in 0..17 {
        usage.mark_billing_incomplete(NOW + offset);
    }
    usage.mark_billing_incomplete(-5);
    let snapshot = usage.snapshot(NOW);
    assert_eq!(
        snapshot.incidents,
        [
            UsageIncident {
                occurred_at_ms: NOW + 16,
                completeness: UsageCompleteness::Incomplete,
            },
            UsageIncident {
                occurred_at_ms: 0,
                completeness: UsageCompleteness::Incomplete,
            },
        ]
    );
    assert_eq!(snapshot.billing, Availability::Incomplete);
}
