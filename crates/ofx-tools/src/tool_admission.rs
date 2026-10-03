use std::path::Path;

use ofx_contract::{ToolOutput, ToolStatusDetail, format_tool_execution_error_json};
use ofx_permissions::FileTargetFailure;
use ofx_workspace::{PathError, resolve_workspace_or_external_path};

pub(crate) fn admit_existing_path(
    tool_name: &str,
    workspace_root: &Path,
    requested: &str,
) -> Result<(), ToolOutput> {
    resolve_workspace_or_external_path(workspace_root, requested)
        .map(drop)
        .map_err(|error| {
            ToolOutput::failure(target_resolution_failure(tool_name, requested, error))
                .with_status_detail(ToolStatusDetail::PreflightFailed)
        })
}

pub(crate) fn admit_optional_path(
    tool_name: &str,
    workspace_root: &Path,
    requested: &str,
) -> Result<(), ToolOutput> {
    if requested.is_empty() || requested == "." {
        return Ok(());
    }
    admit_existing_path(tool_name, workspace_root, requested)
}

pub(crate) fn file_target_failure(tool_name: &str, failure: FileTargetFailure) -> String {
    match failure {
        FileTargetFailure::Resolution(tag) => {
            format!("file mutation target resolution failed: {tag}")
        }
        FileTargetFailure::Operational(error) => {
            format_tool_execution_error_json(tool_name, &error.to_string())
        }
    }
}

fn target_resolution_failure(tool_name: &str, path: &str, error: PathError) -> String {
    let path = path.replace('\0', "\\u0000");
    let reason = match error {
        PathError::FileNotFound => "Path not found",
        PathError::NotDir => "Path is not a directory",
        PathError::AccessDenied | PathError::PermissionDenied => "Access denied for path",
        PathError::PathOutsideWorkspace => "Path is outside the workspace",
        PathError::SymLinkLoop
        | PathError::NameTooLong
        | PathError::BadPathName
        | PathError::InputOutput
        | PathError::HomeNotSet
        | PathError::InvalidPath
        | PathError::WorkspaceUnavailable => {
            return format!("Cannot resolve path \"{path}\": {error}");
        }
        _ => return format_tool_execution_error_json(tool_name, &error.to_string()),
    };
    format!("{reason}: {path}")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn target_resolution_failures_name_the_requested_path() {
        let cases = [
            (PathError::FileNotFound, "Path not found: missing.txt"),
            (PathError::NotDir, "Path is not a directory: missing.txt"),
            (
                PathError::AccessDenied,
                "Access denied for path: missing.txt",
            ),
            (
                PathError::PermissionDenied,
                "Access denied for path: missing.txt",
            ),
            (
                PathError::PathOutsideWorkspace,
                "Path is outside the workspace: missing.txt",
            ),
            (
                PathError::SymLinkLoop,
                "Cannot resolve path \"missing.txt\": SymLinkLoop",
            ),
            (
                PathError::InvalidPath,
                "Cannot resolve path \"missing.txt\": InvalidPath",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(
                target_resolution_failure("read_file", "missing.txt", error),
                expected
            );
        }
        assert_eq!(
            target_resolution_failure("read_file", "missing.txt", PathError::SystemResources),
            "{\"error\":{\"type\":\"tool_execution_failed\",\"tool_name\":\"read_file\",\"message\":\"Tool execution failed\",\"details\":{\"error\":\"SystemResources\"}}}"
        );
    }

    #[test]
    fn file_target_failures_name_the_upstream_tag_or_the_operational_error() {
        assert_eq!(
            file_target_failure(
                "write_file",
                FileTargetFailure::Resolution("file_not_found")
            ),
            "file mutation target resolution failed: file_not_found"
        );
        assert_eq!(
            file_target_failure(
                "write_file",
                FileTargetFailure::Operational(PathError::SystemResources)
            ),
            format_tool_execution_error_json("write_file", "SystemResources")
        );
    }

    #[test]
    fn existing_path_admission_resolves_from_the_workspace_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::create_dir(root.join("workspace")).unwrap();
        fs::write(root.join("workspace/a.txt"), "a").unwrap();
        fs::create_dir(root.join("outside")).unwrap();
        fs::write(root.join("outside/x.txt"), "x").unwrap();
        symlink(root.join("outside"), root.join("workspace/link")).unwrap();
        let workspace = root.join("workspace");

        assert_eq!(
            admit_existing_path("read_file", &workspace, " a.txt "),
            Ok(())
        );
        assert_eq!(
            admit_existing_path("read_file", &workspace, "../outside/x.txt"),
            Ok(())
        );
        assert_eq!(
            admit_existing_path("read_file", &workspace, "link/x.txt"),
            Err(
                ToolOutput::failure("Path is outside the workspace: link/x.txt")
                    .with_status_detail(ToolStatusDetail::PreflightFailed)
            )
        );
        assert_eq!(
            admit_existing_path("read_file", &workspace, "a.txt/x"),
            Err(ToolOutput::failure("Path is not a directory: a.txt/x")
                .with_status_detail(ToolStatusDetail::PreflightFailed))
        );
        assert_eq!(
            admit_existing_path("read_file", &workspace, "../missing.txt"),
            Err(ToolOutput::failure("Path not found: ../missing.txt")
                .with_status_detail(ToolStatusDetail::PreflightFailed))
        );
        assert_eq!(
            admit_existing_path("read_file", &workspace, "a.txt\0x"),
            Err(
                ToolOutput::failure("Cannot resolve path \"a.txt\\u0000x\": InvalidPath")
                    .with_status_detail(ToolStatusDetail::PreflightFailed)
            )
        );
    }
}
