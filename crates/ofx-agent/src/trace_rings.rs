use ofx_trace::{NETWORK_CALLS, NetworkRing, Ring};

use crate::compactor::CompactionEvent;
use crate::compactor::trace::COMPACTION_TRACE;
use crate::tool_call_metrics::{TOOL_CALL_TRACE, ToolCallRing};

#[derive(Clone, Copy)]
pub struct TraceRings {
    pub compaction: &'static Ring<CompactionEvent>,
    pub tool_calls: &'static ToolCallRing,
    pub network: &'static NetworkRing,
}

impl TraceRings {
    pub fn process() -> Self {
        Self {
            compaction: &COMPACTION_TRACE,
            tool_calls: &TOOL_CALL_TRACE,
            network: &NETWORK_CALLS,
        }
    }

    pub fn reset(self) {
        self.compaction.reset();
        self.tool_calls.reset();
        self.network.reset();
    }
}
