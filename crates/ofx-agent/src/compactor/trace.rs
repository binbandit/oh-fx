use std::fmt;

use ofx_trace::{Ring, Sequenced, TraceContext};

const TRACE_SCOPE: &str = "context_compaction";
const RING_CAPACITY: usize = 64;
const MAX_DETAIL_BYTES: usize = 512;

pub(crate) static COMPACTION_TRACE: Ring<CompactionEvent> = Ring::new(RING_CAPACITY);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTraceKind {
    Decision,
    NoCompactableContext,
    OverflowRecoveryIncomplete,
    ProviderOverflowRecovery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionEvent {
    pub timestamp_ms: i64,
    pub context: TraceContext,
    pub failed: bool,
    pub kind: CompactionTraceKind,
    pub detail: String,
    pub truncated: bool,
}

impl CompactionTraceKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::NoCompactableContext => "no_compactable_context",
            Self::OverflowRecoveryIncomplete => "overflow_recovery_incomplete",
            Self::ProviderOverflowRecovery => "provider_overflow_recovery",
        }
    }
}

pub fn compaction_trace() -> Vec<Sequenced<CompactionEvent>> {
    COMPACTION_TRACE.snapshot()
}

pub fn reset_compaction_trace() {
    COMPACTION_TRACE.reset();
}

pub(crate) fn info(
    ring: &Ring<CompactionEvent>,
    context: TraceContext,
    kind: CompactionTraceKind,
    detail: fmt::Arguments<'_>,
) {
    note(ring, context, kind, false, true, detail);
}

pub(crate) fn failure(
    ring: &Ring<CompactionEvent>,
    context: TraceContext,
    kind: CompactionTraceKind,
    detail: fmt::Arguments<'_>,
) {
    note(ring, context, kind, true, true, detail);
}

pub(crate) fn info_if(
    recorded: bool,
    ring: &Ring<CompactionEvent>,
    context: TraceContext,
    kind: CompactionTraceKind,
    detail: fmt::Arguments<'_>,
) {
    note(ring, context, kind, false, recorded, detail);
}

fn note(
    ring: &Ring<CompactionEvent>,
    context: TraceContext,
    kind: CompactionTraceKind,
    failed: bool,
    recorded: bool,
    detail: fmt::Arguments<'_>,
) {
    let traced = ofx_trace::enabled(TRACE_SCOPE);
    if !recorded && !traced {
        return;
    }
    let text = detail.to_string();
    if traced {
        ofx_trace::event(
            TRACE_SCOPE,
            kind.name(),
            context,
            Some(format_args!("{text}")),
        );
    }
    if recorded {
        ring.record(CompactionEvent::new(context, kind, failed, text));
    }
}

impl CompactionEvent {
    fn new(
        context: TraceContext,
        kind: CompactionTraceKind,
        failed: bool,
        mut detail: String,
    ) -> Self {
        let truncated = detail.len() > MAX_DETAIL_BYTES;
        if truncated {
            detail.truncate(detail.floor_char_boundary(MAX_DETAIL_BYTES));
        }
        Self {
            timestamp_ms: ofx_trace::timestamp_ms(),
            context,
            failed,
            kind,
            detail,
            truncated,
        }
    }
}

pub(crate) struct Optional<T>(pub(crate) Option<T>);

impl<T: fmt::Display> fmt::Display for Optional<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(value) => value.fmt(formatter),
            None => formatter.write_str("null"),
        }
    }
}

#[cfg(test)]
mod tests;
