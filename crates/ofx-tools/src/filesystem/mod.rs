mod read_file;

use std::path::PathBuf;

use ofx_contract::{ToolEffect, ToolOutput, ToolSpec};
use serde_json::Value;

pub use read_file::ReadFile;

const DEFAULT_MAX_READ_FILE_LINES: usize = 400;
const DEFAULT_MAX_READ_FILE_LINE_LEN: usize = 2000;

#[derive(Debug, Clone)]
pub(crate) struct FilesystemContext {
    pub(crate) workspace_root: PathBuf,
    pub(crate) max_read_file_lines: usize,
    pub(crate) max_read_file_line_len: usize,
}

impl FilesystemContext {
    pub(crate) fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            max_read_file_lines: DEFAULT_MAX_READ_FILE_LINES,
            max_read_file_line_len: DEFAULT_MAX_READ_FILE_LINE_LEN,
        }
    }
}

fn read_only_effect<T>(decoded: &Result<T, ToolOutput>) -> ToolEffect {
    if decoded.is_ok() {
        ToolEffect::ReadOnly
    } else {
        ToolEffect::None
    }
}

fn tool_spec(name: &str, description: &str, input_schema: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: serde_json::from_str(input_schema).unwrap_or(Value::Null),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use ofx_contract::{CallDescription, Tool, ToolCallId, ToolContext, ToolOutput};
    use tokio_util::sync::CancellationToken;

    use super::*;

    pub(crate) fn run_tool(tool: &dyn Tool, arguments: &str) -> (CallDescription, ToolOutput) {
        let prepared = tool.prepare(arguments).unwrap();
        let description = prepared.describe();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let context = ToolContext::new(ToolCallId::new("call-1"), CancellationToken::new());
        let output = runtime.block_on(prepared.execute(context));
        (description, output)
    }

    #[test]
    fn filesystem_tool_schemas_are_compact_ordered_json() {
        let tools: [&dyn Tool; 1] = [&ReadFile::new("/")];
        for tool in tools {
            let spec = tool.spec();
            assert!(spec.input_schema.is_object(), "{}", spec.name);
            assert!(spec.description.len() <= 1024, "{}", spec.name);
        }
        assert_eq!(
            ReadFile::new("/").spec().input_schema.to_string(),
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy."},"start_line":{"type":"integer","description":"Optional 1-based first line to return. Defaults to 1."},"line_count":{"type":"integer","description":"Optional positive number of lines to return. Defaults to the normal read cap and is bounded."}},"required":["path"]}"#
        );
    }
}
