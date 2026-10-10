use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use ofx_text::content_for_display;

const RING_CAPACITY: usize = 64;
const MAX_NAME_BYTES: usize = 64;
const MAX_ARGS_BYTES: usize = 1200;
const MAX_RESULT_BYTES: usize = 2000;
const PAYLOAD_FREE_TOOL: &str = "web_fetch";

pub(crate) static TOOL_CALL_TRACE: ToolCallRing = ToolCallRing::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallOutcome {
    Succeeded,
    Rejected,
    CommandFailed,
    ToolFailed,
    RuntimeFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallMetric {
    pub started_at_ms: i64,
    pub duration_ms: u32,
    pub outcome: ToolCallOutcome,
    pub subagent_id: u64,
    pub name: String,
    pub args: String,
    pub args_total_bytes: u32,
    pub result: String,
    pub result_total_bytes: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCallLifetime {
    pub total_calls: u64,
    pub total_duration_ms: u64,
    pub outcome_counts: [u64; ToolCallOutcome::ALL.len()],
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCallTrace {
    pub calls: Vec<ToolCallMetric>,
    pub lifetime: ToolCallLifetime,
}

pub(crate) struct ToolCallRecord<'a> {
    pub(crate) name: &'a str,
    pub(crate) arguments: &'a str,
    pub(crate) output: &'a str,
    pub(crate) outcome: ToolCallOutcome,
    pub(crate) started_at_ms: i64,
    pub(crate) finished_at_ms: i64,
    pub(crate) subagent_id: u64,
}

pub(crate) struct ToolCallRing {
    state: Mutex<State>,
}

struct State {
    calls: VecDeque<ToolCallMetric>,
    lifetime: ToolCallLifetime,
}

impl ToolCallOutcome {
    pub const ALL: [Self; 5] = [
        Self::Succeeded,
        Self::Rejected,
        Self::CommandFailed,
        Self::ToolFailed,
        Self::RuntimeFailed,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Rejected => "rejected",
            Self::CommandFailed => "command_failed",
            Self::ToolFailed => "tool_failed",
            Self::RuntimeFailed => "runtime_failed",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

impl ToolCallLifetime {
    pub const fn count_for(&self, outcome: ToolCallOutcome) -> u64 {
        self.outcome_counts[outcome.index()]
    }
}

pub fn tool_call_trace() -> ToolCallTrace {
    TOOL_CALL_TRACE.snapshot()
}

pub fn reset_tool_call_trace() {
    TOOL_CALL_TRACE.reset();
}

impl ToolCallRing {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                calls: VecDeque::new(),
                lifetime: ToolCallLifetime {
                    total_calls: 0,
                    total_duration_ms: 0,
                    outcome_counts: [0; ToolCallOutcome::ALL.len()],
                },
            }),
        }
    }

    pub(crate) fn record(&self, record: &ToolCallRecord<'_>) {
        let elapsed = record.finished_at_ms.saturating_sub(record.started_at_ms);
        let duration_ms = u32::try_from(elapsed.max(0)).unwrap_or(u32::MAX);
        let (args, args_total_bytes, result, result_total_bytes) =
            if record.name == PAYLOAD_FREE_TOOL {
                (String::new(), 0, String::new(), 0)
            } else {
                let shown = content_for_display(record.output);
                (
                    cut(record.arguments, MAX_ARGS_BYTES),
                    total_bytes(record.arguments),
                    cut(shown, MAX_RESULT_BYTES),
                    total_bytes(shown),
                )
            };
        self.push(ToolCallMetric {
            started_at_ms: record.started_at_ms,
            duration_ms,
            outcome: record.outcome,
            subagent_id: record.subagent_id,
            name: cut(record.name, MAX_NAME_BYTES),
            args,
            args_total_bytes,
            result,
            result_total_bytes,
        });
    }

    fn push(&self, metric: ToolCallMetric) {
        let mut state = self.lock();
        let lifetime = &mut state.lifetime;
        lifetime.total_calls = lifetime.total_calls.saturating_add(1);
        lifetime.outcome_counts[metric.outcome.index()] =
            lifetime.outcome_counts[metric.outcome.index()].saturating_add(1);
        lifetime.total_duration_ms = lifetime
            .total_duration_ms
            .saturating_add(u64::from(metric.duration_ms));
        if state.calls.len() == RING_CAPACITY {
            state.calls.pop_front();
        }
        state.calls.push_back(metric);
    }

    pub(crate) fn snapshot(&self) -> ToolCallTrace {
        let state = self.lock();
        ToolCallTrace {
            calls: state.calls.iter().cloned().collect(),
            lifetime: state.lifetime,
        }
    }

    pub(crate) fn reset(&self) {
        let mut state = self.lock();
        state.calls.clear();
        state.lifetime = ToolCallLifetime::default();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn cut(text: &str, max_bytes: usize) -> String {
    text[..text.floor_char_boundary(max_bytes)].to_owned()
}

fn total_bytes(text: &str) -> u32 {
    u32::try_from(text.len()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
