use std::sync::Arc;

use crate::tool_dispatch::Tool;

#[derive(Clone, Copy)]
pub struct ToolSet<'a> {
    pub tools: &'a [Arc<dyn Tool>],
    pub read_only_tool_names: &'a [&'a str],
}

impl ToolSet<'_> {
    pub(crate) fn registers(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool.spec().name == name)
    }

    pub(crate) fn is_read_only(&self, name: &str) -> bool {
        self.read_only_tool_names.contains(&name)
    }
}
