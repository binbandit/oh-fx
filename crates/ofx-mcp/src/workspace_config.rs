use std::env;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::mcp_contract::WorkspaceAdmission;
use crate::project_config::{
    ProjectMcpChoices, WorkspaceDiagnostic, WorkspaceDiagnosticCause, WorkspaceParseResult,
    expand_approved_workspace_configs, parse_workspace_document,
};

pub const WORKSPACE_CONFIG_FILE_NAME: &str = ".mcp.json";
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

pub fn load_workspace_config(
    workspace_root: &Path,
    choices: &ProjectMcpChoices,
) -> io::Result<WorkspaceParseResult> {
    load_workspace_config_with_environment(workspace_root, choices, &|name| env::var(name).ok())
}

pub fn load_workspace_config_with_environment(
    workspace_root: &Path,
    choices: &ProjectMcpChoices,
    environment: &dyn Fn(&str) -> Option<String>,
) -> io::Result<WorkspaceParseResult> {
    match fs::metadata(workspace_root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(WorkspaceParseResult::default()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(WorkspaceParseResult::default());
        }
        Err(error) => return Err(error),
    }
    let path = workspace_root.join(WORKSPACE_CONFIG_FILE_NAME);
    let bytes = match read_regular_file(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(WorkspaceParseResult::default()),
        Err(_) => return Ok(invalid_result()),
    };
    let mut result = parse_workspace_document(&bytes, choices);
    let has_approved = result
        .configs
        .iter()
        .any(|config| config.workspace_admission == Some(WorkspaceAdmission::Approved));
    if has_approved {
        expand_approved_workspace_configs(&mut result, environment);
    }
    Ok(result)
}

fn read_regular_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let stat = rustix::fs::fstat(&fd)?;
    let oversized = u64::try_from(stat.st_size).map_or(true, |size| size > MAX_CONFIG_BYTES);
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || oversized
    {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let mut bytes = Vec::new();
    File::from(fd)
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(Some(bytes))
}

fn invalid_result() -> WorkspaceParseResult {
    WorkspaceParseResult {
        configs: Vec::new(),
        diagnostics: vec![WorkspaceDiagnostic::new(
            WorkspaceDiagnosticCause::InvalidEntry,
        )],
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn approved(names: &[&str]) -> ProjectMcpChoices {
        ProjectMcpChoices {
            approved: names.iter().map(|name| (*name).to_owned()).collect(),
            ..ProjectMcpChoices::default()
        }
    }

    #[test]
    fn workspace_mcp_loading_expands_command_args_environment_and_http_headers() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join(WORKSPACE_CONFIG_FILE_NAME),
            r#"{"mcpServers":{"local":{"command":"${CMD}","args":["--root=${ROOT:-/srv}"],"env":{"TOKEN":"${TOKEN}"}},"remote":{"type":"http","url":"https://example.test/mcp","headers":{"X-Token":"${TOKEN}"}}}}"#,
        )
        .unwrap();
        let lookup = |name: &str| match name {
            "CMD" => Some("node".to_owned()),
            "TOKEN" => Some("secret".to_owned()),
            _ => None,
        };
        let result = load_workspace_config_with_environment(
            root.path(),
            &approved(&["local", "remote"]),
            &lookup,
        )
        .unwrap();
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.configs[0].command.as_deref(), Some("node"));
        assert_eq!(result.configs[0].args, vec!["--root=/srv".to_owned()]);
        assert_eq!(result.configs[0].env[0].value, "secret");
        assert_eq!(result.configs[1].headers[0].value, "secret");
    }

    #[test]
    fn missing_roots_and_files_yield_nothing() {
        let root = tempfile::tempdir().unwrap();
        let choices = ProjectMcpChoices::default();
        assert_eq!(
            load_workspace_config(root.path(), &choices).unwrap(),
            WorkspaceParseResult::default()
        );
        assert_eq!(
            load_workspace_config(&root.path().join("absent"), &choices).unwrap(),
            WorkspaceParseResult::default()
        );
    }

    #[test]
    fn symlinked_or_oversized_files_become_one_invalid_entry() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("real.json");
        fs::write(&target, r#"{"mcpServers":{}}"#).unwrap();
        symlink(&target, root.path().join(WORKSPACE_CONFIG_FILE_NAME)).unwrap();
        let choices = ProjectMcpChoices::default();
        assert_eq!(
            load_workspace_config(root.path(), &choices).unwrap(),
            invalid_result()
        );

        let oversized = tempfile::tempdir().unwrap();
        fs::write(
            oversized.path().join(WORKSPACE_CONFIG_FILE_NAME),
            vec![b' '; 1024 * 1024 + 1],
        )
        .unwrap();
        assert_eq!(
            load_workspace_config(oversized.path(), &choices).unwrap(),
            invalid_result()
        );
    }

    #[test]
    fn hard_linked_files_become_one_invalid_entry() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside.json");
        fs::write(&outside, r#"{"mcpServers":{}}"#).unwrap();
        fs::hard_link(&outside, root.path().join(WORKSPACE_CONFIG_FILE_NAME)).unwrap();
        assert_eq!(
            load_workspace_config(root.path(), &ProjectMcpChoices::default()).unwrap(),
            invalid_result()
        );
    }

    #[test]
    fn pending_servers_never_read_environment_values() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join(WORKSPACE_CONFIG_FILE_NAME),
            r#"{"mcpServers":{"local":{"command":"${SECRET}"}}}"#,
        )
        .unwrap();
        let lookup =
            |_: &str| -> Option<String> { panic!("environment read for a pending server") };
        let result = load_workspace_config_with_environment(
            root.path(),
            &ProjectMcpChoices::default(),
            &lookup,
        )
        .unwrap();
        assert_eq!(result.configs[0].command.as_deref(), Some("${SECRET}"));
    }
}
