use super::*;

fn context(turn_id: u64, step_id: u64) -> TraceContext {
    TraceContext {
        turn_id,
        step_id,
        subagent_id: 0,
    }
}

#[test]
fn compaction_diagnostics_stay_bounded_and_reset_without_file_tracing() {
    let ring = Ring::new(RING_CAPACITY);
    info(
        &ring,
        context(7, 3),
        CompactionTraceKind::Decision,
        format_args!("automatic_threshold tokens={}/{}", 279_466, 280_000),
    );
    let oversized = "x".repeat(MAX_DETAIL_BYTES + 10);
    failure(
        &ring,
        context(7, 4),
        CompactionTraceKind::NoCompactableContext,
        format_args!("{oversized}"),
    );
    let events = ring.snapshot();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event.kind.name(), "decision");
    assert_eq!(
        events[0].event.detail,
        "automatic_threshold tokens=279466/280000"
    );
    assert!(!events[0].event.failed);
    assert_eq!(events[0].event.context, context(7, 3));
    assert_eq!(events[1].event.kind.name(), "no_compactable_context");
    assert!(events[1].event.failed);
    assert!(events[1].event.truncated);
    assert!(events[1].event.detail.len() <= MAX_DETAIL_BYTES);
    ring.reset();
    assert!(ring.snapshot().is_empty());
    info(
        &ring,
        context(8, 0),
        CompactionTraceKind::ProviderOverflowRecovery,
        format_args!("model={}", "model-a"),
    );
    let events = ring.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, 1);
}

#[test]
fn a_decision_that_keeps_the_context_is_left_out_of_the_ring() {
    let ring = Ring::new(RING_CAPACITY);
    info_if(
        false,
        &ring,
        context(1, 1),
        CompactionTraceKind::Decision,
        format_args!("decision=no_op"),
    );
    info_if(
        true,
        &ring,
        context(1, 2),
        CompactionTraceKind::Decision,
        format_args!("decision=compact"),
    );
    let events = ring.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.detail, "decision=compact");
}

#[test]
fn a_cut_detail_ends_on_a_character_boundary() {
    let ring = Ring::new(RING_CAPACITY);
    let detail = format!("{}é", "x".repeat(MAX_DETAIL_BYTES - 1));
    info(
        &ring,
        TraceContext::default(),
        CompactionTraceKind::Decision,
        format_args!("{detail}"),
    );
    let event = &ring.snapshot()[0].event;
    assert!(event.truncated);
    assert_eq!(event.detail.len(), MAX_DETAIL_BYTES - 1);
}

#[test]
fn missing_values_print_as_null() {
    assert_eq!(Optional(Some(5)).to_string(), "5");
    assert_eq!(Optional::<usize>(None).to_string(), "null");
}
