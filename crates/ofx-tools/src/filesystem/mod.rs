mod edit_file;
mod glob_files;
mod grep_files;
mod read_file;
mod write_file;

use std::fmt::Write;
use std::path::PathBuf;

use ofx_contract::{ToolEffect, ToolOutput, ToolSpec};
use ofx_workspace::{CandidateStats, DEFAULT_CANDIDATE_CAP, IGNORED_DIRECTORY_NAMES};

pub use edit_file::EditFile;
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

pub(crate) fn tool_spec(name: &str, description: &str, input_schema: &'static str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::process::Command;

    use ofx_contract::{
        Admission, CallDescription, GatedAction, PathAccess, PermissionGate, PermissionMode,
        PreparedCall, Tool, ToolCall, ToolCallId, ToolContext, ToolOutput,
    };
    use ofx_permissions::PermissionPolicy;
    use ofx_workspace::GIT_REPOSITORY_VARIABLES;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    pub(crate) struct RememberedGrant {
        _temp: TempDir,
        root: PathBuf,
        pub(crate) workspace: PathBuf,
    }

    impl RememberedGrant {
        pub(crate) fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap();
            let workspace = root.join("workspace");
            fs::create_dir(&workspace).unwrap();
            for (name, content) in [
                ("allowed/a.txt", "needle allowed\n"),
                ("secret/a.txt", "needle secret\n"),
                ("secret/credentials.txt", "needle secret\n"),
            ] {
                let path = root.join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, content).unwrap();
            }
            symlink(root.join("allowed"), root.join("link")).unwrap();
            Self {
                _temp: temp,
                root,
                workspace,
            }
        }

        pub(crate) fn run_around_a_swap(
            &self,
            tool: &dyn Tool,
            arguments: &str,
        ) -> (ToolOutput, ToolOutput) {
            let call = ToolCall {
                id: ToolCallId::new("call-1"),
                name: tool.spec().name.clone(),
                arguments: arguments.to_owned(),
            };
            let policy = PermissionPolicy::new(PermissionMode::Ask, &self.workspace);
            assert_eq!(policy.admit(&call), Admission::ApprovalRequired);
            policy.remember_approval(GatedAction::Call(&call));
            let admitted = || match policy.admit(&call) {
                Admission::Allowed(path_access) => path_access,
                other => panic!("the remembered approval admits the call: {other:?}"),
            };
            let (_, before) = run_tool_with(tool, arguments, admitted());
            let path_access = admitted();
            let mut prepared = tool.prepare(arguments).unwrap();
            prepared.complete();
            fs::remove_file(self.root.join("link")).unwrap();
            symlink(self.root.join("secret"), self.root.join("link")).unwrap();
            assert_eq!(policy.admit(&call), Admission::ApprovalRequired);
            let after = execute(prepared, path_access);
            for disclosed in ["secret", "credentials"] {
                assert!(!after.content.contains(disclosed), "{}", after.content);
            }
            (before, after)
        }
    }

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
        let mut prepared = tool.prepare(arguments).unwrap();
        prepared.complete();
        let description = prepared.describe();
        (description, execute(prepared, path_access))
    }

    fn execute(prepared: Box<dyn PreparedCall>, path_access: PathAccess) -> ToolOutput {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let context = ToolContext::new(
            ToolCallId::new("call-1"),
            CancellationToken::new(),
            path_access,
        );
        runtime.block_on(prepared.execute(context))
    }

    #[test]
    fn read_file_keeps_the_upstream_schema() {
        assert_eq!(
            ReadFile::new("/").spec().input_schema,
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy."},"start_line":{"type":"integer","description":"Optional 1-based first line to return. Defaults to 1."},"line_count":{"type":"integer","description":"Optional positive number of lines to return. Defaults to the normal read cap and is bounded."}},"required":["path"]}"#
        );
    }
}
