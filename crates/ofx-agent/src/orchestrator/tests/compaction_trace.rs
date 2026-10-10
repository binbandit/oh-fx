use ofx_trace::{Ring, Sequenced};

use super::compaction::{spoken_tool_reply, unmetered, windowed};
use super::*;
use crate::compactor::{CompactionEvent, CompactionTraceKind};

fn traced(agent: &mut Agent) -> &'static Ring<CompactionEvent> {
    let ring: &'static Ring<CompactionEvent> = Box::leak(Box::new(Ring::new(64)));
    agent.compaction_trace = ring;
    ring
}

fn kinds(events: &[Sequenced<CompactionEvent>]) -> Vec<(CompactionTraceKind, bool)> {
    events
        .iter()
        .map(|event| (event.event.kind, event.event.failed))
        .collect()
}

fn overflow() -> Script {
    let mut error = failure(ProviderErrorKind::InvalidRequest, "BadRequest");
    error.detail = Some("maximum context length exceeded".to_owned());
    Script::Fail(Vec::new(), error)
}

#[tokio::test]
async fn an_automatic_compaction_records_its_decision_with_the_turn_and_step() {
    let big_reply = "h".repeat(150_000);
    let provider = FakeProvider::new(vec![
        spoken_tool_reply("Reading first.", "call-1", r#"{"value":"first.txt"}"#),
        unmetered(text_reply(&big_reply)),
        unmetered(text_reply("Turn 1\nIn between: Finish the read.")),
        unmetered(text_reply("Automatic compaction complete.")),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let ring = traced(&mut agent);
    run(&mut agent, "first").await;
    assert!(ring.snapshot().is_empty());
    let (report, _) = run(&mut agent, "second").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let events = ring.snapshot();
    assert_eq!(kinds(&events), [(CompactionTraceKind::Decision, false)]);
    let decision = &events[0].event;
    assert_ne!(decision.context.turn_id, 0);
    assert_ne!(decision.context.step_id, 0);
    assert_eq!(decision.context.subagent_id, 0);
    assert!(
        decision
            .detail
            .starts_with("decision=compact overflow=false request_bytes="),
        "{}",
        decision.detail
    );
    assert!(decision.detail.contains(" has_images=false image_baseline=false prior_input_tokens=null usable_tokens=44936 compact_at_tokens=35948 compact_at_percent=80 max_output_tokens=64"), "{}", decision.detail);
}

#[tokio::test]
async fn a_context_overflow_records_its_recovery_and_the_compaction_it_asks_for() {
    let provider = FakeProvider::new(vec![
        spoken_tool_reply("", "prior-read", r#"{"value":"notes"}"#),
        unmetered(text_reply("prior assistant")),
        overflow(),
        unmetered(text_reply(
            "Turn 1\nIn between: Compact the prior turn once.",
        )),
        unmetered(text_reply("RECOVERED")),
    ]);
    let (mut agent, _) = windowed(&provider, 128_000, 16_384);
    let ring = traced(&mut agent);
    run(&mut agent, "prior user").await;
    let (report, _) = run(&mut agent, "continue").await;
    assert_eq!(report.final_text, "RECOVERED");
    let events = ring.snapshot();
    assert_eq!(
        kinds(&events),
        [
            (CompactionTraceKind::ProviderOverflowRecovery, false),
            (CompactionTraceKind::Decision, false),
        ]
    );
    assert!(
        events[0]
            .event
            .detail
            .starts_with("model=test-model request_bytes="),
        "{}",
        events[0].event.detail
    );
    assert!(
        events[1]
            .event
            .detail
            .starts_with("decision=compact overflow=true ")
    );
    assert_eq!(
        events[0].event.context.turn_id,
        events[1].event.context.turn_id
    );
    assert!(events[0].event.context.step_id < events[1].event.context.step_id);
}

#[tokio::test]
async fn a_request_that_still_does_not_fit_records_that_nothing_is_left_to_compact() {
    let provider = FakeProvider::new(vec![unmetered(text_reply("small answer"))]);
    let (mut agent, _) = windowed(&provider, 2_000, 64);
    let ring = traced(&mut agent);
    run(&mut agent, "small question").await;
    let (report, _) = run(&mut agent, &"x ".repeat(4_000)).await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    let events = ring.snapshot();
    assert_eq!(
        kinds(&events),
        [
            (CompactionTraceKind::Decision, false),
            (CompactionTraceKind::NoCompactableContext, true),
        ]
    );
    assert!(
        events[1].event.detail.starts_with("estimated_tokens="),
        "{}",
        events[1].event.detail
    );
    assert!(events[1].event.detail.ends_with(" usable_tokens=1936"));
}
