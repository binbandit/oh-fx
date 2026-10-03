use std::path::PathBuf;

use ofx_contract::{
    CallPresentation, LivePermissionMode, PreparedCall, Tool, ToolActivity, ToolOutput, ToolSpec,
};
use ofx_workspace::ChangeTracker;

use super::tool_spec;
use crate::file_mutation::{MAX_CONTENT_BYTES, MutationInput, path_limit_failure};
use crate::file_mutation_execution::MutationRequest;
use crate::tool_args::{parse_arguments, required_string};

const TOOL_NAME: &str = "edit_file";
const DESCRIPTION: &str = "Edit an existing file by replacing one exact old_string occurrence with new_string. Paths may be workspace-relative or external using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. When to use: make a focused patch after reading the file. When NOT to use: broad rewrites, ambiguous repeated text, generated formatting, missing files, or cross-file refactors.";
const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy."},"old_string":{"type":"string","description":"Exact text to find in the file. Must match exactly once."},"new_string":{"type":"string","description":"Text to replace old_string with."}},"required":["path","old_string","new_string"]}"#;
const PRESENTATION: CallPresentation = CallPresentation {
    activity: ToolActivity::Edit,
    action_label: "Editing",
    completed_label: "Edited",
    label_argument: "path",
    label_default: "file",
};

pub struct EditFile {
    spec: ToolSpec,
    request: MutationRequest,
}

impl EditFile {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            spec: tool_spec(TOOL_NAME, DESCRIPTION, INPUT_SCHEMA),
            request: MutationRequest {
                tool_name: TOOL_NAME,
                presentation: PRESENTATION,
                workspace_root: workspace_root.into(),
                permission_mode: None,
                change_tracker: None,
            },
        }
    }

    #[must_use]
    pub fn with_permission_mode(mut self, permission_mode: LivePermissionMode) -> Self {
        self.request.permission_mode = Some(permission_mode);
        self
    }

    #[must_use]
    pub fn with_change_tracker(mut self, change_tracker: ChangeTracker) -> Self {
        self.request.change_tracker = Some(change_tracker);
        self
    }
}

impl Tool for EditFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        Ok(self.request.prepare(decode(arguments)))
    }
}

fn decode(arguments: &str) -> Result<(String, MutationInput), ToolOutput> {
    let arguments = parse_arguments(TOOL_NAME, arguments)?;
    let path = required_string(TOOL_NAME, &arguments, "path")?;
    let old_string = required_string(TOOL_NAME, &arguments, "old_string")?;
    let new_string = required_string(TOOL_NAME, &arguments, "new_string")?;
    if let Some(failure) = path_limit_failure(&path) {
        return Err(failure);
    }
    for (field, value) in [("old_string", &old_string), ("new_string", &new_string)] {
        if value.len() > MAX_CONTENT_BYTES {
            return Err(ToolOutput::failure(format!(
                "edit_file failed: {field} exceeds the 4 MiB preparation limit"
            )));
        }
    }
    Ok((
        path,
        MutationInput::Edit {
            old_string,
            new_string,
        },
    ))
}

#[cfg(test)]
mod tests;
