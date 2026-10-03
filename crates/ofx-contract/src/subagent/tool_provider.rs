use super::model_contract::SubagentRequest;
use crate::stream_provider::BoxFuture;
use crate::tool_dispatch::{ToolContext, ToolOutput};

pub trait SubagentProvider: Send + Sync {
    fn execute(
        &self,
        request: SubagentRequest,
        context: ToolContext,
    ) -> BoxFuture<'static, ToolOutput>;
}
