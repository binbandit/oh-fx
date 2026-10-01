use std::path::{Path, PathBuf};

use ofx_contract::{ToolCall, parse_tool_args_object};
use ofx_workspace::resolve_workspace_or_external_path;

mod file_mutation_targets;

pub use file_mutation_targets::{
    FileMutationKind, FileMutationTargets, FileTargetFailure, TraversalDirectory,
    prepare_file_mutation_targets,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionTargetKind {
    PathExisting,
    PathOptionalExisting,
}

const EXTERNAL_PATH_TOOLS: [(&str, PermissionTargetKind); 3] = [
    ("read_file", PermissionTargetKind::PathExisting),
    ("glob_files", PermissionTargetKind::PathOptionalExisting),
    ("grep_files", PermissionTargetKind::PathOptionalExisting),
];

pub(crate) fn external_path_target(workspace_root: &Path, call: &ToolCall) -> Option<PathBuf> {
    let (_, kind) = EXTERNAL_PATH_TOOLS
        .iter()
        .find(|(name, _)| *name == call.name)?;
    let arguments = parse_tool_args_object(&call.arguments).ok()?;
    match (kind, arguments.optional_string("path")) {
        (PermissionTargetKind::PathOptionalExisting, None | Some("" | ".")) => {
            Some(workspace_root.to_path_buf())
        }
        (_, Some(path)) => resolve_workspace_or_external_path(workspace_root, path).ok(),
        (PermissionTargetKind::PathExisting, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ofx_contract::ToolCallId;

    use super::*;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("call-1"),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    #[test]
    fn file_tool_targets_resolve_like_the_tools_and_other_calls_have_none() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("secret.txt"), "secret\n").unwrap();

        for (name, arguments, target) in [
            (
                "read_file",
                r#"{"path":" src/main.rs "}"#,
                "workspace/src/main.rs",
            ),
            ("read_file", r#"{"path":"../secret.txt"}"#, "secret.txt"),
            ("glob_files", r#"{"pattern":"*"}"#, "workspace"),
            ("glob_files", r#"{"pattern":"*","path":7}"#, "workspace"),
            ("glob_files", r#"{"pattern":"*","path":""}"#, "workspace"),
            ("grep_files", r#"{"pattern":"x","path":"."}"#, "workspace"),
            (
                "grep_files",
                r#"{"pattern":"x","path":"src"}"#,
                "workspace/src",
            ),
            ("grep_files", r#"{"pattern":"x","path":".."}"#, ""),
        ] {
            assert_eq!(
                external_path_target(&workspace, &call(name, arguments)),
                Some(root.join(target).components().collect()),
                "{name} {arguments}"
            );
        }
        for (name, arguments) in [
            ("read_file", r#"{"path":"missing.txt"}"#),
            ("read_file", r#"{"path":7}"#),
            ("read_file", "{"),
            ("grep_files", r#"{"pattern":"x","path":"missing"}"#),
            ("unknown_tool", r#"{"path":"../secret.txt"}"#),
        ] {
            assert_eq!(
                external_path_target(&workspace, &call(name, arguments)),
                None,
                "{name} {arguments}"
            );
        }
    }
}
