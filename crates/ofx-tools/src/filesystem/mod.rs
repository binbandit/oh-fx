mod glob_files;
mod grep_files;
mod read_file;
mod write_file;

use std::fmt::Write;
use std::path::PathBuf;

use ofx_contract::{ToolEffect, ToolOutput, ToolSpec};
use ofx_workspace::{CandidateStats, DEFAULT_CANDIDATE_CAP, IGNORED_DIRECTORY_NAMES};
use serde_json::Value;

pub use glob_files::GlobFiles;
pub use grep_files::GrepFiles;
pub use read_file::ReadFile;
pub use write_file::WriteFile;

const DEFAULT_MAX_LIST_ENTRIES: usize = 100;
const DEFAULT_MAX_READ_FILE_LINES: usize = 400;
const DEFAULT_MAX_READ_FILE_LINE_LEN: usize = 2000;

#[derive(Debug, Clone)]
pub(crate) struct FilesystemContext {
    pub(crate) workspace_root: PathBuf,
    pub(crate) ignored_list_entries: &'static [&'static str],
    pub(crate) max_list_entries: usize,
    pub(crate) max_read_file_lines: usize,
    pub(crate) max_read_file_line_len: usize,
}

impl FilesystemContext {
    pub(crate) fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            ignored_list_entries: IGNORED_DIRECTORY_NAMES,
            max_list_entries: DEFAULT_MAX_LIST_ENTRIES,
            max_read_file_lines: DEFAULT_MAX_READ_FILE_LINES,
            max_read_file_line_len: DEFAULT_MAX_READ_FILE_LINE_LEN,
        }
    }
}

fn render_candidate_notes(candidates: &CandidateStats) -> String {
    let mut notes = String::new();
    if candidates.incomplete {
        let _ = writeln!(
            notes,
            "... candidate list may be incomplete; candidate cap {DEFAULT_CANDIDATE_CAP} reached before all files were discovered"
        );
    }
    if candidates.skipped_overlong > 0 {
        let plural = if candidates.skipped_overlong == 1 {
            ""
        } else {
            "s"
        };
        let _ = writeln!(
            notes,
            "... skipped {} overlong candidate path{plural}",
            candidates.skipped_overlong
        );
    }
    notes
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
    use std::path::Path;
    use std::process::Command;

    use ofx_contract::{CallDescription, PathAccess, Tool, ToolCallId, ToolContext, ToolOutput};
    use ofx_workspace::GIT_REPOSITORY_VARIABLES;
    use tokio_util::sync::CancellationToken;

    use super::*;

    pub(crate) fn run_git(root: &Path, args: &[&str]) -> bool {
        let mut command = Command::new("git");
        command.args(args).current_dir(root);
        for name in GIT_REPOSITORY_VARIABLES {
            command.env_remove(name);
        }
        command.output().is_ok_and(|output| output.status.success())
    }

    pub(crate) fn run_tool(tool: &dyn Tool, arguments: &str) -> (CallDescription, ToolOutput) {
        run_tool_with(tool, arguments, PathAccess::WorkspaceOnly)
    }

    pub(crate) fn run_tool_with(
        tool: &dyn Tool,
        arguments: &str,
        path_access: PathAccess,
    ) -> (CallDescription, ToolOutput) {
        let prepared = tool.prepare(arguments).unwrap();
        let description = prepared.describe();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let context = ToolContext::new(
            ToolCallId::new("call-1"),
            CancellationToken::new(),
            path_access,
        );
        let output = runtime.block_on(prepared.execute(context));
        (description, output)
    }

    #[test]
    fn filesystem_tool_schemas_are_compact_ordered_json() {
        let tools: [&dyn Tool; 3] = [
            &ReadFile::new("/"),
            &GlobFiles::new("/"),
            &GrepFiles::new("/"),
        ];
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
