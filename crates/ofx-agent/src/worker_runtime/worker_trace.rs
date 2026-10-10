use ofx_trace::{TraceContext, trace_event, trace_log};

use super::{Boundary, BoundaryKind, QueuedPrompt, TurnSettings};

const WORKER: &str = "worker";

pub(super) struct Admitted {
    pub(super) turn_id: u64,
    pub(super) prompt_bytes: usize,
    pub(super) targeted: bool,
    pub(super) plain: bool,
    pub(super) interrupt_model: bool,
}

fn turn(turn_id: u64) -> TraceContext {
    TraceContext {
        turn_id,
        ..TraceContext::default()
    }
}

pub(super) fn enqueued(admitted: &Admitted, queue_depth: usize, settings: &TurnSettings) {
    let TurnSettings { fast_mode, effort } = settings;
    let bytes = admitted.prompt_bytes;
    trace_log!(
        WORKER,
        "queued prompt bytes={bytes} queue_depth={queue_depth} fast_mode={fast_mode} effort={effort}"
    );
    trace_event!(
        WORKER,
        "prompt_enqueue",
        turn(admitted.turn_id),
        "prompt_bytes={bytes} queue_depth={queue_depth} fast_mode={fast_mode} effort={effort}"
    );
}

pub(super) fn steering_admission(
    admitted: &Admitted,
    (active_turn, tool_boundary, compaction_active): (u64, bool, bool),
) {
    trace_event!(
        WORKER,
        "steering_admission",
        turn(active_turn),
        "queued_turn_id={} targeted={} plain={} tool_boundary={tool_boundary} compaction_active={compaction_active} interrupt_model={}",
        admitted.turn_id,
        admitted.targeted,
        admitted.plain,
        admitted.interrupt_model
    );
}

pub(super) fn began(prompt: &QueuedPrompt, remaining: usize) {
    let TurnSettings { fast_mode, effort } = &prompt.settings;
    let bytes = prompt.text.len();
    trace_log!(
        WORKER,
        "begin prompt bytes={bytes} remaining_queue={remaining} fast_mode={fast_mode} effort={effort}"
    );
    trace_event!(
        WORKER,
        "worker_begin",
        turn(prompt.turn_id),
        "prompt_bytes={bytes} remaining_queue={remaining} cancel_reset=true fast_mode={fast_mode} effort={effort}"
    );
}

pub(super) fn boundary_checked(active_turn: u64, kind: BoundaryKind, boundary: &Boundary) {
    let kind = match kind {
        BoundaryKind::Model => "model",
        BoundaryKind::Cancelled => "cancelled",
        BoundaryKind::Finalizing => "finalizing",
    };
    let outcome = match boundary {
        Boundary::None => "none",
        Boundary::Continue(_) => "continue_turn",
        Boundary::Handoff => "handoff",
        Boundary::Interrupt => "interrupt",
    };
    trace_event!(
        WORKER,
        "steering_boundary_check",
        turn(active_turn),
        "kind={kind} outcome={outcome}"
    );
}

pub(super) fn steering_consumed(active_turn: u64, count: usize) {
    trace_event!(
        WORKER,
        "prompt_steering_consumed",
        turn(active_turn),
        "count={count}"
    );
}

pub(super) fn steering_deferred(finished_turn: u64, prompt: &QueuedPrompt) {
    trace_event!(
        WORKER,
        "steering_deferred_to_next_turn",
        turn(finished_turn),
        "queued_turn_id={} prompt_bytes={}",
        prompt.turn_id,
        prompt.text.len()
    );
}

pub(super) fn removed(turn_id: u64, remaining: usize) {
    trace_event!(
        WORKER,
        "queued_prompt_removed",
        turn(turn_id),
        "remaining={remaining}"
    );
}

pub(super) fn retracted(turn_id: u64, remaining: usize) {
    trace_event!(
        WORKER,
        "prompt_steering_retracted",
        turn(turn_id),
        "remaining={remaining}"
    );
}
