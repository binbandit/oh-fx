use std::path::{Path, PathBuf};

use ofx_contract::{ToolCall, parse_tool_args_object};
use ofx_workspace::resolve_workspace_or_external_path;

const EXTERNAL_PATH_TOOLS: [&str; 1] = ["read_file"];

pub(crate) fn external_path_target(workspace_root: &Path, call: &ToolCall) -> Option<PathBuf> {
    if !EXTERNAL_PATH_TOOLS.contains(&call.name.as_str()) {
        return None;
    }
    let arguments = parse_tool_args_object(&call.arguments).ok()?;
    let path = arguments.optional_string("path")?;
    resolve_workspace_or_external_path(workspace_root, path).ok()
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
    fn read_file_targets_resolve_like_the_tool_and_other_calls_have_none() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("secret.txt"), "secret\n").unwrap();

        assert_eq!(
            external_path_target(
                &workspace,
                &call("read_file", r#"{"path":" src/main.rs "}"#)
            ),
            Some(workspace.join("src/main.rs"))
        );
        assert_eq!(
            external_path_target(
                &workspace,
                &call("read_file", r#"{"path":"../secret.txt"}"#)
            ),
            Some(root.join("secret.txt"))
        );
        for (name, arguments) in [
            ("read_file", r#"{"path":"missing.txt"}"#),
            ("read_file", r#"{"path":7}"#),
            ("read_file", "{"),
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
