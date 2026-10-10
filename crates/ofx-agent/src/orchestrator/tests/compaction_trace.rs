use ofx_trace::{Ring, Sequenced, TraceContext};

use super::compaction::{chat, chat_replies, metered, spoken_tool_reply, unmetered, windowed};
use super::*;
use crate::compactor::{CompactionError, CompactionEvent, CompactionTraceKind};

pub(super) fn traced(agent: &mut Agent) -> &'static Ring<CompactionEvent> {
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

fn details(events: &[Sequenced<CompactionEvent>]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event.event.detail.as_str())
        .collect()
}

fn named(events: &[Sequenced<CompactionEvent>], kind: CompactionTraceKind) -> Vec<&str> {
    events
        .iter()
        .filter(|event| event.event.kind == kind)
        .map(|event| event.event.detail.as_str())
        .collect()
}

const LOG: (CompactionTraceKind, bool) = (CompactionTraceKind::Log, false);

fn bad_request() -> ProviderError {
    ProviderError {
        status: Some(400),
        ..failure(ProviderErrorKind::InvalidRequest, "BadRequest")
    }
}

fn overflow() -> Script {
    let mut error = failure(ProviderErrorKind::InvalidRequest, "BadRequest");
    error.detail = Some("maximum context length exceeded".to_owned());
    Script::Fail(Vec::new(), error)
}

#[tokio::test]
async fn an_automatic_compaction_records_each_stage_with_the_turn_and_step() {
    let big_reply = "h".repeat(150_000);
    let provider = FakeProvider::new(vec![
        spoken_tool_reply("Reading first.", "call-1", r#"{"value":"first.txt"}"#),
        unmetered(text_reply(&big_reply)),
        unmetered(text_reply("Turn 1\nIn between: Finish the read.")),
        metered(text_reply("Automatic compaction complete."), Some(4_321)),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let ring = traced(&mut agent);
    run(&mut agent, "first").await;
    assert!(ring.snapshot().is_empty());
    let (report, _) = run(&mut agent, "second").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let events = ring.snapshot();
    assert_eq!(
        kinds(&events),
        [
            (CompactionTraceKind::Decision, false),
            LOG,
            (CompactionTraceKind::ProviderStart, false),
            LOG,
            LOG,
            LOG,
            (CompactionTraceKind::ProviderCompleted, false),
            (CompactionTraceKind::Committed, false),
            (CompactionTraceKind::Installed, false),
            LOG,
            LOG,
        ]
    );
    let decision = &events[0].event;
    assert_ne!(decision.context.turn_id, 0);
    assert_ne!(decision.context.step_id, 0);
    assert_eq!(decision.context.subagent_id, 0);
    assert_eq!(
        details(&events),
        [
            "decision=compact overflow=false request_bytes=150965 estimated_tokens=37756 text_tokens=37756 has_images=false image_baseline=false prior_input_tokens=null usable_tokens=44936 compact_at_tokens=35948 compact_at_percent=80 max_output_tokens=64",
            "room after compaction after_tokens=8987 fixed_tokens=100 kept_tokens=3554 kept_used=10 compacted_tokens=17864",
            "model=test-model turns=1 earlier=false store=false",
            "compaction model call model=test-model after_conversation=true input_tokens=null cache_read_tokens=null cache_write_tokens=null output_tokens=2",
            "compaction notes: turns_noted=1/1 tool_notes=0 tools=1 entries=0 earlier_bytes=0 reply_bytes=35 after_conversation=true",
            "compacted text over its room; its longest texts were clipped, whole in their saved turns tokens=37614 clipped_tokens=9498 limit=17864 clip_bytes=37500",
            "model=test-model summaries=1 shown_turns=1 turns=1 tools=1 entries=0 used=0 text_bytes=37966 fallback=none",
            "origin=automatic removed_turns=1 compaction_count=1 summary_bytes=37966 tools=1",
            "request_bytes_before=150965 estimated_tokens_before=37756 summary_bytes=37966 tools=1",
            "request after compaction estimated_tokens=9647 fixed_tokens=100 usable_tokens=44936 after_tokens=8987",
            "request after compaction exact_input_tokens=4321 estimated_tokens=9647",
        ]
    );
    for event in [&events[2], &events[6], &events[7], &events[8]] {
        assert_eq!(event.event.context, decision.context);
    }
    assert_eq!(events[1].event.context, TraceContext::default());
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
            LOG,
            (CompactionTraceKind::ProviderStart, false),
            (CompactionTraceKind::Log, true),
            LOG,
            LOG,
            (CompactionTraceKind::Log, true),
            (CompactionTraceKind::ProviderCompleted, false),
            (CompactionTraceKind::Committed, false),
            (CompactionTraceKind::Installed, false),
            LOG,
        ]
    );
    assert_eq!(
        details(&events)[4..],
        [
            "ledger request over its limit after clipping tokens=598 limit=192",
            "compaction model call model=test-model after_conversation=false input_tokens=null cache_read_tokens=null cache_write_tokens=null output_tokens=2",
            "compaction notes: turns_noted=1/1 tool_notes=0 tools=1 entries=0 earlier_bytes=0 reply_bytes=47 after_conversation=false",
            "compacted text over its room with no text long enough to clip tokens=122 limit=22",
            "model=test-model summaries=1 shown_turns=1 turns=1 tools=1 entries=0 used=0 text_bytes=464 fallback=none",
            "origin=provider_overflow removed_turns=1 compaction_count=1 summary_bytes=464 tools=1",
            "request_bytes_before=973 estimated_tokens_before=257 summary_bytes=464 tools=1",
            "request after compaction estimated_tokens=273 fixed_tokens=100 usable_tokens=111616 after_tokens=22323",
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
            LOG,
            (CompactionTraceKind::ProviderStart, false),
            (CompactionTraceKind::ProviderCompleted, false),
            (CompactionTraceKind::Committed, false),
            (CompactionTraceKind::Installed, false),
            LOG,
            (CompactionTraceKind::NoCompactableContext, true),
        ]
    );
    assert_eq!(
        details(&events)[6..],
        [
            "request after compaction estimated_tokens=4245 fixed_tokens=100 usable_tokens=1936 after_tokens=387",
            "estimated_tokens=4245 usable_tokens=1936",
        ]
    );
}

#[tokio::test]
async fn a_manual_compaction_records_its_own_turn_and_counts_every_compaction() {
    let provider = FakeProvider::new(chat_replies(12));
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let ring = traced(&mut agent);
    chat(&mut agent, 6).await;
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    let events = ring.snapshot();
    assert_eq!(
        kinds(&events),
        [
            LOG,
            (CompactionTraceKind::ProviderStart, false),
            (CompactionTraceKind::ProviderCompleted, false),
            (CompactionTraceKind::Committed, false),
        ]
    );
    let shown = details(&events);
    assert!(
        shown[0].starts_with(
            "room after compaction after_tokens=18446744073709551615 fixed_tokens=null kept_tokens="
        ),
        "{}",
        shown[0]
    );
    assert_eq!(
        shown[1],
        "model=test-model turns=2 earlier=false store=false"
    );
    assert!(
        shown[2].starts_with("model=test-model summaries=0 shown_turns=2 turns=2 tools=0 entries=0 used=0 text_bytes="),
        "{}",
        shown[2]
    );
    assert!(
        shown[3].starts_with("origin=manual removed_turns=2 compaction_count=1 summary_bytes="),
        "{}",
        shown[3]
    );
    let context = events[1].event.context;
    assert_ne!(context.turn_id, 0);
    assert_eq!(context.step_id, 0);
    assert_eq!(events[3].event.context, context);

    chat(&mut agent, 6).await;
    ring.reset();
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    let again = ring.snapshot();
    assert_eq!(
        named(&again, CompactionTraceKind::ProviderStart),
        ["model=test-model turns=6 earlier=true store=false"]
    );
    let committed = named(&again, CompactionTraceKind::Committed);
    assert!(
        committed[0].contains(" compaction_count=2 "),
        "{}",
        committed[0]
    );
    assert!(ring.snapshot()[1].event.context.turn_id > context.turn_id);
}

#[tokio::test]
async fn a_resumed_session_counts_the_compactions_before_it() {
    let provider = FakeProvider::new(chat_replies(6));
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let ring = traced(&mut agent);
    agent.restore(ofx_contract::RestoredHistory {
        compaction_count: 3,
        ..ofx_contract::RestoredHistory::default()
    });
    chat(&mut agent, 6).await;
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    let events = ring.snapshot();
    assert!(named(&events, CompactionTraceKind::Committed)[0].contains(" compaction_count=4 "));
}

#[tokio::test]
async fn a_manual_compaction_with_nothing_to_compact_records_a_no_op_decision() {
    let provider = FakeProvider::new(Vec::new());
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let ring = traced(&mut agent);
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Unchanged)
    );
    let events = ring.snapshot();
    assert_eq!(kinds(&events), [(CompactionTraceKind::Decision, false)]);
    assert_eq!(
        details(&events),
        ["decision=no_op origin=manual reason=nothing_to_compact"]
    );
}

#[tokio::test]
async fn a_checkpoint_that_cannot_be_saved_records_a_failed_publication() {
    let provider = FakeProvider::new(chat_replies(6));
    let log = Box::new(turn_log::MemoryLog {
        refused_checkpoint: Some("SessionCommitFailed"),
        ..turn_log::MemoryLog::default()
    });
    let mut agent = turn_log::logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    let ring = traced(&mut agent);
    chat(&mut agent, 6).await;
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Err(CompactionError::NotSaved)
    );
    let events = ring.snapshot();
    let last = &events.last().expect("a failed publication").event;
    assert_eq!(last.kind, CompactionTraceKind::TransactionFailed);
    assert!(last.failed);
    assert_eq!(
        last.detail,
        "stage=publication origin=manual err=SessionCommitFailed"
    );
    assert!(named(&events, CompactionTraceKind::Committed).is_empty());
}

#[tokio::test]
async fn a_failed_automatic_compaction_records_each_failed_request_and_its_stage() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply(&big_step, "call-1", r#"{"value":"notes.md"}"#),
        Script::Fail(Vec::new(), bad_request()),
        Script::Fail(Vec::new(), bad_request()),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let ring = traced(&mut agent);
    let (report, _) = run(&mut agent, "read the notes").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    let events = ring.snapshot();
    assert_eq!(
        kinds(&events),
        [
            (CompactionTraceKind::Decision, false),
            LOG,
            (CompactionTraceKind::ProviderStart, false),
            LOG,
            (CompactionTraceKind::SummaryTransportFailed, true),
            (CompactionTraceKind::Log, true),
            LOG,
            (CompactionTraceKind::SummaryTransportFailed, true),
            (CompactionTraceKind::TransactionFailed, true),
        ]
    );
    let shown = details(&events);
    assert_eq!(shown[4], "model=test-model kind=invalid_request detail=");
    assert_eq!(
        shown[5],
        "compaction notes after the conversation failed err=ModelFailed; writing the turns out"
    );
    assert!(
        shown[6].contains(" after_conversation=false "),
        "{}",
        shown[6]
    );
    assert_eq!(shown[8], "stage=summary origin=automatic err=ModelFailed");
}

#[tokio::test]
async fn a_cancelled_automatic_compaction_is_not_a_problem() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply(&big_step, "call-1", r#"{"value":"notes.md"}"#),
        Script::WaitForCancel,
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let ring = traced(&mut agent);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let watched = Arc::clone(&provider);
    tokio::spawn(async move {
        while watched.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
        trigger.cancel();
    });
    let report = agent.run_turn("read the notes", &mut |_| {}, &cancel).await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    let events = ring.snapshot();
    let last = &events.last().expect("a cancelled compaction").event;
    assert_eq!(last.kind, CompactionTraceKind::TransactionFailed);
    assert!(!last.failed);
    assert_eq!(last.detail, "stage=summary origin=automatic err=Cancelled");
}
