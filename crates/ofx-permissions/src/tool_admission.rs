use std::path::PathBuf;

use ofx_contract::{Admission, PathAccess, PermissionGate, PermissionMode, ToolCall};
use ofx_workspace::path_inside;

use crate::permissions::external_path_target;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPolicy {
    mode: PermissionMode,
    workspace_root: PathBuf,
}

impl PermissionPolicy {
    pub fn new(mode: PermissionMode, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            workspace_root: workspace_root.into(),
        }
    }
}

impl PermissionGate for PermissionPolicy {
    fn admit(&self, call: &ToolCall) -> Admission {
        if self.mode == PermissionMode::Yolo {
            return Admission::Allowed(PathAccess::WorkspaceOrExternal);
        }
        match external_path_target(&self.workspace_root, call) {
            Some(target) if !path_inside(&self.workspace_root, &target) => {
                Admission::ApprovalRequired
            }
            _ => Admission::Allowed(PathAccess::WorkspaceOnly),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use ofx_contract::ToolCallId;

    use super::*;

    fn read(path: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("call-1"),
            name: "read_file".to_owned(),
            arguments: format!(r#"{{"path":"{path}"}}"#),
        }
    }

    fn admissions(mode: PermissionMode, workspace: &Path, paths: &[&str]) -> Vec<Admission> {
        let policy = PermissionPolicy::new(mode, workspace);
        paths.iter().map(|path| policy.admit(&read(path))).collect()
    }

    #[test]
    fn reads_outside_the_workspace_need_approval_unless_full_access_is_on() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::create_dir(root.join("outside")).unwrap();
        fs::write(root.join("outside/secret.txt"), "secret\n").unwrap();
        symlink(root.join("outside"), workspace.join("link")).unwrap();
        let external = root.join("outside/secret.txt");
        let external = external.to_str().unwrap();
        let reentry = workspace.join("src/main.rs");
        let paths = [
            "src/main.rs",
            reentry.to_str().unwrap(),
            "../workspace/src/main.rs",
            "missing.txt",
            "link/secret.txt",
            external,
            "../outside/secret.txt",
        ];
        let workspace_only = Admission::Allowed(PathAccess::WorkspaceOnly);

        for mode in [PermissionMode::Ask, PermissionMode::Auto] {
            assert_eq!(
                admissions(mode, &workspace, &paths),
                [
                    workspace_only,
                    workspace_only,
                    workspace_only,
                    workspace_only,
                    workspace_only,
                    Admission::ApprovalRequired,
                    Admission::ApprovalRequired,
                ],
                "{mode:?}"
            );
        }
        assert_eq!(
            admissions(PermissionMode::Yolo, &workspace, &paths),
            [Admission::Allowed(PathAccess::WorkspaceOrExternal); 7]
        );
    }

    #[test]
    fn calls_without_an_external_path_target_stay_inside_the_workspace() {
        let policy = PermissionPolicy::new(PermissionMode::Ask, "/");
        let call = ToolCall {
            id: ToolCallId::new("call-1"),
            name: "unknown_tool".to_owned(),
            arguments: r#"{"path":"/etc/hosts"}"#.to_owned(),
        };
        assert_eq!(
            policy.admit(&call),
            Admission::Allowed(PathAccess::WorkspaceOnly)
        );
    }
}
