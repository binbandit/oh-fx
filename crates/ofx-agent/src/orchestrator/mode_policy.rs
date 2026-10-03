use std::sync::Arc;

use ofx_contract::{ActiveMode, Tool, ToolOutput, ToolSet, ToolSpec};

pub(super) struct ModePolicy {
    mode: ActiveMode,
    advertised: Vec<ToolSpec>,
}

impl ModePolicy {
    pub(super) fn new(mode: ActiveMode, tools: &[Arc<dyn Tool>]) -> Self {
        let advertised = mode
            .registry
            .model_tools(&tool_set(mode, tools), mode.id)
            .iter()
            .map(|tool| tool.spec().clone())
            .collect();
        Self { mode, advertised }
    }

    pub(super) fn advertised(&self) -> &[ToolSpec] {
        &self.advertised
    }

    pub(super) fn denial(&self, tools: &[Arc<dyn Tool>], tool_name: &str) -> Option<ToolOutput> {
        self.mode
            .registry
            .tool_policy_denied_json(&tool_set(self.mode, tools), self.mode.id, tool_name)
            .map(ToolOutput::failure)
    }
}

fn tool_set(mode: ActiveMode, tools: &[Arc<dyn Tool>]) -> ToolSet<'_> {
    ToolSet {
        tools,
        read_only_tool_names: mode.read_only_tool_names,
    }
}
