use super::*;

fn call(duration_ms: u32) -> NetworkCall {
    NetworkCall {
        duration_ms,
        ..NetworkCall::default()
    }
}

fn on_turn(turn_id: u64, duration_ms: u32, started_at_ms: i64) -> NetworkCall {
    NetworkCall {
        started_at_ms,
        duration_ms,
        turn_id,
        ..NetworkCall::default()
    }
}

#[test]
fn the_ring_keeps_the_last_calls_in_chronological_order() {
    let ring = NetworkRing::new();
    for index in 0..RING_CAPACITY + 5 {
        ring.record(NetworkCall {
            model: "anthropic/claude-opus-4.6".to_owned(),
            ..call(u32::try_from(index).unwrap())
        });
    }
    let trace = ring.snapshot();
    assert_eq!(trace.calls.len(), RING_CAPACITY);
    assert_eq!(trace.calls[0].duration_ms, 5);
    assert_eq!(
        trace.calls[RING_CAPACITY - 1].duration_ms,
        u32::try_from(RING_CAPACITY + 4).unwrap()
    );
}

#[test]
fn lifetime_totals_cover_evicted_calls_and_a_reset_clears_them() {
    let ring = NetworkRing::new();
    for index in 0..RING_CAPACITY + 5 {
        ring.record(NetworkCall {
            started_at_ms: 1000 + i64::try_from(index).unwrap() * 100,
            status: if index % 7 == 0 { 500 } else { 0 },
            ..call(10)
        });
    }
    let trace = ring.snapshot();
    let total = u64::try_from(RING_CAPACITY + 5).unwrap();
    assert_eq!(trace.lifetime.total_calls, total);
    assert!(trace.lifetime.error_calls > 0);
    assert_eq!(
        trace.lifetime.ok_calls + trace.lifetime.error_calls,
        trace.lifetime.total_calls
    );
    assert_eq!(trace.lifetime.total_duration_ms, total * 10);
    assert!(trace.lifetime.total_calls > u64::try_from(trace.calls.len()).unwrap());
    ring.reset();
    assert_eq!(ring.snapshot(), NetworkTrace::default());
}

#[test]
fn turn_rollups_sum_each_turn_and_evict_the_coldest_turn() {
    let ring = NetworkRing::new();
    ring.record(on_turn(2, 10, 1000));
    ring.record(NetworkCall {
        subagent_id: 7,
        error: "Timeout".to_owned(),
        ..on_turn(2, 20, 2000)
    });
    ring.record(on_turn(2, 30, 3000));
    ring.record(on_turn(9, 40, 4000));
    ring.record(on_turn(0, 50, 5000));
    let trace = ring.snapshot();
    assert_eq!(
        trace.turns,
        [
            NetworkTurnRollup {
                turn_id: 2,
                calls: 3,
                error_calls: 1,
                subagent_calls: 1,
                total_duration_ms: 60,
                first_started_at_ms: 1000,
            },
            NetworkTurnRollup {
                turn_id: 9,
                calls: 1,
                error_calls: 0,
                subagent_calls: 0,
                total_duration_ms: 40,
                first_started_at_ms: 4000,
            },
        ]
    );
    assert_eq!(trace.lifetime.total_calls, 5);
    assert_eq!(trace.lifetime.evicted_turns, 0);

    let first = 100;
    let last = first + u64::try_from(TURN_ROLLUP_CAPACITY).unwrap();
    for turn_id in first..last {
        ring.record(on_turn(turn_id, 1, 6000));
    }
    let trace = ring.snapshot();
    assert_eq!(trace.turns.len(), TURN_ROLLUP_CAPACITY);
    assert!(
        trace
            .turns
            .iter()
            .all(|rollup| rollup.turn_id != 2 && rollup.turn_id != 9)
    );
    assert!(
        trace
            .turns
            .windows(2)
            .all(|pair| pair[0].turn_id < pair[1].turn_id)
    );
    assert_eq!(trace.turns[0].turn_id, first);
    assert_eq!(trace.turns[TURN_ROLLUP_CAPACITY - 1].turn_id, last - 1);
    assert_eq!(trace.lifetime.evicted_turns, 2);
}

#[test]
fn recorded_text_is_cut_at_its_limit_on_a_character_boundary() {
    let ring = NetworkRing::new();
    ring.record(NetworkCall {
        model: format!("{}é", "m".repeat(MAX_MODEL_BYTES - 1)),
        error: "e".repeat(MAX_ERROR_BYTES + 8),
        stop_reason: "s".repeat(MAX_STOP_REASON_BYTES + 8),
        ..NetworkCall::default()
    });
    let recorded = &ring.snapshot().calls[0];
    assert_eq!(recorded.model, "m".repeat(MAX_MODEL_BYTES - 1));
    assert_eq!(recorded.error.len(), MAX_ERROR_BYTES);
    assert_eq!(recorded.stop_reason.len(), MAX_STOP_REASON_BYTES);
}

#[test]
fn a_call_is_an_error_with_an_error_name_or_an_error_status() {
    assert!(!call(1).is_error());
    assert!(
        !NetworkCall {
            status: 399,
            ..call(1)
        }
        .is_error()
    );
    assert!(
        NetworkCall {
            status: 400,
            ..call(1)
        }
        .is_error()
    );
    assert!(
        NetworkCall {
            error: "ConnectionFailed".to_owned(),
            ..call(1)
        }
        .is_error()
    );
    assert_eq!(NetworkCallKind::Gateway.name(), "gateway");
    assert_eq!(NetworkCallKind::WebFetchTarget.name(), "web_fetch_target");
}
