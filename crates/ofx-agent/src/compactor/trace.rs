use std::fmt;

use ofx_trace::{Ring, Sequenced, TraceContext};

const TRACE_SCOPE: &str = "context_compaction";
const RING_CAPACITY: usize = 64;
const MAX_DETAIL_BYTES: usize = 512;

pub(crate) static COMPACTION_TRACE: Ring<CompactionEvent> = Ring::new(RING_CAPACITY);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTraceKind {
    Log,
    ProviderStart,
    ProviderCompleted,
    SummaryTransportFailed,
    SummaryIncomplete,
    SummaryToolCallRejected,
    SummaryTruncated,
    TransactionFailed,
    Committed,
    Decision,
    NoCompactableContext,
    RetentionExhausted,
    RetentionForcedZero,
    Installed,
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

#[derive(Clone, Copy)]
pub(crate) struct Tracer {
    ring: &'static Ring<CompactionEvent>,
    context: TraceContext,
}

#[derive(Clone, Copy)]
enum Line {
    Event(CompactionTraceKind),
    Log,
}

impl CompactionTraceKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::ProviderStart => "provider_start",
            Self::ProviderCompleted => "provider_completed",
            Self::SummaryTransportFailed => "summary_transport_failed",
            Self::SummaryIncomplete => "summary_incomplete",
            Self::SummaryToolCallRejected => "summary_tool_call_rejected",
            Self::SummaryTruncated => "summary_truncated",
            Self::TransactionFailed => "transaction_failed",
            Self::Committed => "committed",
            Self::Decision => "decision",
            Self::NoCompactableContext => "no_compactable_context",
            Self::RetentionExhausted => "retention_exhausted",
            Self::RetentionForcedZero => "retention_forced_zero",
            Self::Installed => "installed",
            Self::OverflowRecoveryIncomplete => "overflow_recovery_incomplete",
            Self::ProviderOverflowRecovery => "provider_overflow_recovery",
        }
    }
}

pub(crate) fn unrecorded(detail: fmt::Arguments<'_>) {
    ofx_trace::log(TRACE_SCOPE, detail);
}

pub fn compaction_trace() -> Vec<Sequenced<CompactionEvent>> {
    COMPACTION_TRACE.snapshot()
}

pub fn reset_compaction_trace() {
    COMPACTION_TRACE.reset();
}

impl Tracer {
    pub(crate) const fn new(ring: &'static Ring<CompactionEvent>, context: TraceContext) -> Self {
        Self { ring, context }
    }

    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self::new(
            Box::leak(Box::new(Ring::new(RING_CAPACITY))),
            TraceContext::default(),
        )
    }

    pub(crate) fn info(self, kind: CompactionTraceKind, detail: fmt::Arguments<'_>) {
        self.note(Line::Event(kind), false, true, detail);
    }

    pub(crate) fn failure(self, kind: CompactionTraceKind, detail: fmt::Arguments<'_>) {
        self.note(Line::Event(kind), true, true, detail);
    }

    pub(crate) fn info_if(
        self,
        recorded: bool,
        kind: CompactionTraceKind,
        detail: fmt::Arguments<'_>,
    ) {
        self.note(Line::Event(kind), false, recorded, detail);
    }

    pub(crate) fn log(self, failed: bool, detail: fmt::Arguments<'_>) {
        self.note(Line::Log, failed, true, detail);
    }

    fn note(self, line: Line, failed: bool, recorded: bool, detail: fmt::Arguments<'_>) {
        let traced = ofx_trace::enabled(TRACE_SCOPE);
        if !recorded && !traced {
            return;
        }
        let text = detail.to_string();
        let (kind, context) = match line {
            Line::Event(kind) => {
                if traced {
                    ofx_trace::event(
                        TRACE_SCOPE,
                        kind.name(),
                        self.context,
                        Some(format_args!("{text}")),
                    );
                }
                (kind, self.context)
            }
            Line::Log => {
                if traced {
                    ofx_trace::log(TRACE_SCOPE, format_args!("{text}"));
                }
                (CompactionTraceKind::Log, TraceContext::default())
            }
        };
        if recorded {
            self.ring
                .record(CompactionEvent::new(context, kind, failed, text));
        }
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
