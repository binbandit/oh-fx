use std::path::PathBuf;

use ofx_contract::{CallPresentation, PreparedCall, Tool, ToolActivity, ToolOutput, ToolSpec};
use ofx_workspace::MAX_PATH_BYTES;

use super::tool_spec;
use crate::file_mutation::{MAX_CONTENT_BYTES, MutationInput};
use crate::file_mutation_execution::MutationRequest;
use crate::tool_args::{parse_arguments, required_string};

const TOOL_NAME: &str = "write_file";
const DESCRIPTION: &str = "Create or overwrite a file using complete contents. Paths may be workspace-relative or external using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. When to use: add a new file or intentionally replace an entire generated/small file. When NOT to use: targeted edits to existing files, partial replacements, deleting files, or unapproved external paths.";
const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy."},"content":{"type":"string","description":"Complete file contents to write."}},"required":["path","content"]}"#;
const PRESENTATION: CallPresentation = CallPresentation {
    activity: ToolActivity::Write,
    action_label: "Writing",
    label_argument: "path",
    label_default: "file",
};
const PATH_LIMIT_FAILURE: &str =
    "file mutation preparation failed: path exceeds the preparation limit";

pub struct WriteFile {
    spec: ToolSpec,
    request: MutationRequest,
}

impl WriteFile {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            spec: tool_spec(TOOL_NAME, DESCRIPTION, INPUT_SCHEMA),
            request: MutationRequest {
                tool_name: TOOL_NAME,
                presentation: PRESENTATION,
                workspace_root: workspace_root.into(),
            },
        }
    }
}

impl Tool for WriteFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        Ok(self.request.prepare(arguments, decode(arguments)))
    }
}

fn decode(arguments: &str) -> Result<(String, MutationInput), ToolOutput> {
    let arguments = parse_arguments(TOOL_NAME, arguments)?;
    let path = required_string(TOOL_NAME, &arguments, "path")?;
    let content = required_string(TOOL_NAME, &arguments, "content")?;
    if path.len() > MAX_PATH_BYTES {
        return Err(ToolOutput::failure(PATH_LIMIT_FAILURE));
    }
    if content.len() > MAX_CONTENT_BYTES {
        return Err(ToolOutput::failure(
            "write_file failed: content exceeds the 4 MiB preparation limit",
        ));
    }
    Ok((path, MutationInput::Write(content)))
}

#[cfg(test)]
mod tests;
