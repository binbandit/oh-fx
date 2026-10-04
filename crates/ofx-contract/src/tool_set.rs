use crate::tool_dispatch::ToolSpec;

#[derive(Clone, Copy)]
pub struct ToolSet<'a> {
    pub specs: &'a [ToolSpec],
    pub read_only_tool_names: &'a [&'a str],
}

impl ToolSet<'_> {
    pub(crate) fn registers(&self, name: &str) -> bool {
        self.specs.iter().any(|spec| spec.name == name)
    }

    pub(crate) fn is_read_only(&self, name: &str) -> bool {
        self.read_only_tool_names.contains(&name)
    }
}
