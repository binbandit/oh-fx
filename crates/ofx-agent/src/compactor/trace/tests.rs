use super::*;

fn tracer(turn_id: u64, step_id: u64) -> (Tracer, &'static Ring<CompactionEvent>) {
    let ring: &'static Ring<CompactionEvent> = Box::leak(Box::new(Ring::new(RING_CAPACITY)));
    let context = TraceContext {
        turn_id,
        step_id,
        subagent_id: 0,
    };
    (Tracer::new(ring, context), ring)
}

#[test]
fn compaction_diagnostics_stay_bounded_and_reset_without_file_tracing() {
    let (decided, ring) = tracer(7, 3);
    decided.info(
        CompactionTraceKind::Decision,
        format_args!("automatic_threshold tokens={}/{}", 279_466, 280_000),
    );
    let oversized = "x".repeat(MAX_DETAIL_BYTES + 10);
    Tracer::new(
        ring,
        TraceContext {
            step_id: 4,
            ..decided.context
        },
    )
    .failure(
        CompactionTraceKind::RetentionExhausted,
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
    assert_eq!(events[0].event.context.turn_id, 7);
    assert_eq!(events[0].event.context.step_id, 3);
    assert_eq!(events[1].event.kind.name(), "retention_exhausted");
    assert!(events[1].event.failed);
    assert!(events[1].event.truncated);
    assert!(events[1].event.detail.len() <= MAX_DETAIL_BYTES);
    ring.reset();
    assert!(ring.snapshot().is_empty());
    Tracer::new(
        ring,
        TraceContext {
            turn_id: 8,
            ..TraceContext::default()
        },
    )
    .info(
        CompactionTraceKind::Installed,
        format_args!("kept_users={}", 3),
    );
    let events = ring.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, 1);
}

#[test]
fn a_decision_that_keeps_the_context_is_left_out_of_the_ring() {
    let (traced, ring) = tracer(1, 1);
    traced.info_if(
        false,
        CompactionTraceKind::Decision,
        format_args!("decision=no_op"),
    );
    traced.info_if(
        true,
        CompactionTraceKind::Decision,
        format_args!("decision=compact"),
    );
    let events = ring.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.detail, "decision=compact");
}

#[test]
fn a_free_form_note_is_recorded_without_its_turn() {
    let (traced, ring) = tracer(9, 2);
    traced.log(
        false,
        format_args!("room after compaction after_tokens={}", 10),
    );
    traced.log(true, format_args!("record store {} failed", "read"));
    let events = ring.snapshot();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event.kind, CompactionTraceKind::Log);
    assert_eq!(events[0].event.kind.name(), "log");
    assert_eq!(events[0].event.context, TraceContext::default());
    assert_eq!(
        events[0].event.detail,
        "room after compaction after_tokens=10"
    );
    assert!(!events[0].event.failed);
    assert!(events[1].event.failed);
}

#[test]
fn a_cut_detail_ends_on_a_character_boundary() {
    let (traced, ring) = tracer(0, 0);
    let detail = format!("{}é", "x".repeat(MAX_DETAIL_BYTES - 1));
    traced.info(CompactionTraceKind::Decision, format_args!("{detail}"));
    let event = &ring.snapshot()[0].event;
    assert!(event.truncated);
    assert_eq!(event.detail.len(), MAX_DETAIL_BYTES - 1);
}

#[test]
fn missing_values_print_as_null() {
    assert_eq!(Optional(Some(5)).to_string(), "5");
    assert_eq!(Optional::<usize>(None).to_string(), "null");
}
