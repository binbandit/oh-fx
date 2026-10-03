use std::fmt;
use std::sync::Arc;

use super::model_contract::SubagentRequest;
use crate::stream_provider::BoxFuture;
use crate::tool_dispatch::{ToolContext, ToolOutput};
use crate::types::ReasoningEffort;

pub trait SubagentProvider: Send + Sync {
    fn execute(
        &self,
        request: SubagentRequest,
        context: ToolContext,
    ) -> BoxFuture<'static, ToolOutput>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentStatus {
    pub model: String,
    pub effort: ReasoningEffort,
}

#[derive(Clone)]
pub struct SubagentStatusSink(Arc<dyn Fn(SubagentStatus) + Send + Sync>);

impl SubagentStatusSink {
    pub fn new(publish: impl Fn(SubagentStatus) + Send + Sync + 'static) -> Self {
        Self(Arc::new(publish))
    }

    pub fn publish(&self, status: SubagentStatus) {
        (self.0)(status);
    }
}

impl fmt::Debug for SubagentStatusSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SubagentStatusSink")
    }
}
