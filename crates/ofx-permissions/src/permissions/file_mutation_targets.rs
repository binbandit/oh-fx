use std::os::fd::OwnedFd;
use std::path::Path;

use ofx_workspace::{
    FileIdentity, FileKind, FileMutationTarget, PathError, TargetMode, descriptor_identity,
    entry_identity, open_child_directory, open_directory, resolve_file_mutation_target,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileMutationKind {
    Write,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TraversalDirectory {
    Existing(FileIdentity),
    Create,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMutationTargets {
    pub target: FileMutationTarget,
    pub anchor_identity: FileIdentity,
    pub traversal: Vec<TraversalDirectory>,
    pub target_identity: Option<FileIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTargetFailure {
    Resolution(&'static str),
    Operational(PathError),
}

impl From<PathError> for FileTargetFailure {
    fn from(error: PathError) -> Self {
        Self::Resolution(match error {
            PathError::PathOutsideWorkspace => "path_outside_workspace",
            PathError::FileNotFound => "file_not_found",
            PathError::HomeNotSet => "home_not_set",
            PathError::InvalidPath => "invalid_path",
            PathError::WorkspaceUnavailable => "workspace_unavailable",
            PathError::TooManyPathComponents => "too_many_path_components",
            PathError::AccessDenied | PathError::PermissionDenied => "access_denied",
            PathError::NotDir => "not_directory",
            PathError::SymLinkLoop => "symlink_loop",
            PathError::NameTooLong => "name_too_long",
            PathError::BadPathName => "bad_path_name",
            PathError::IsDir | PathError::PathAlreadyExists => "path_state_changed",
            PathError::InputOutput
            | PathError::NoSpaceLeft
            | PathError::ReadOnlyFileSystem
            | PathError::DeviceBusy
            | PathError::FileBusy
            | PathError::FileTooBig
            | PathError::WouldBlock
            | PathError::NoDevice => "filesystem_unavailable",
            PathError::SystemResources
            | PathError::OutOfMemory
            | PathError::ProcessFdQuotaExceeded
            | PathError::SystemFdQuotaExceeded
            | PathError::Unexpected => return Self::Operational(error),
        })
    }
}

pub fn prepare_file_mutation_targets(
    workspace_root: &Path,
    path: &str,
    kind: FileMutationKind,
) -> Result<FileMutationTargets, FileTargetFailure> {
    let mode = match kind {
        FileMutationKind::Write => TargetMode::Create,
        FileMutationKind::Edit => TargetMode::Existing,
    };
    let target = resolve_file_mutation_target(workspace_root, path, mode)?;
    let Some((name, parents)) = target.components.split_last() else {
        return Err(FileTargetFailure::Resolution("invalid_path"));
    };
    let anchor = open_directory(&target.anchor)?;
    let anchor_identity = directory_identity(&anchor)?;
    let mut current = Some(anchor);
    let mut traversal = Vec::with_capacity(parents.len());
    for parent in parents {
        let Some(directory) = current.take() else {
            traversal.push(TraversalDirectory::Create);
            continue;
        };
        match open_child_directory(&directory, parent) {
            Err(PathError::FileNotFound) if kind == FileMutationKind::Write => {
                traversal.push(TraversalDirectory::Create);
            }
            Err(error) => return Err(error.into()),
            Ok(child) => {
                traversal.push(TraversalDirectory::Existing(directory_identity(&child)?));
                current = Some(child);
            }
        }
    }
    let target_identity = match current {
        None => None,
        Some(directory) => match entry_identity(&directory, name) {
            Ok(identity) => Some(identity),
            Err(PathError::FileNotFound) if kind == FileMutationKind::Write => None,
            Err(error) => return Err(error.into()),
        },
    };
    Ok(FileMutationTargets {
        target,
        anchor_identity,
        traversal,
        target_identity,
    })
}

fn directory_identity(directory: &OwnedFd) -> Result<FileIdentity, FileTargetFailure> {
    let identity = descriptor_identity(directory)?;
    if identity.kind() == FileKind::Directory {
        Ok(identity)
    } else {
        Err(FileTargetFailure::Resolution("not_directory"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
        workspace: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap();
            let workspace = root.join("workspace");
            fs::create_dir_all(workspace.join("src")).unwrap();
            fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
            Self {
                _temp: temp,
                root,
                workspace,
            }
        }

        fn prepare(
            &self,
            path: &str,
            kind: FileMutationKind,
        ) -> Result<FileMutationTargets, FileTargetFailure> {
            prepare_file_mutation_targets(&self.workspace, path, kind)
        }
    }

    #[test]
    fn existing_targets_record_every_directory_and_the_target_identity() {
        let fixture = Fixture::new();
        let targets = fixture
            .prepare("src/main.rs", FileMutationKind::Edit)
            .unwrap();

        assert_eq!(targets.target.anchor, fixture.workspace);
        assert_eq!(targets.anchor_identity.kind(), FileKind::Directory);
        assert!(matches!(
            targets.traversal[..],
            [TraversalDirectory::Existing(identity)] if identity.kind() == FileKind::Directory
        ));
        assert_eq!(
            targets.target_identity.map(FileIdentity::kind),
            Some(FileKind::RegularFile)
        );
        assert_ne!(targets.traversal[0], TraversalDirectory::Create);
    }

    #[test]
    fn missing_parents_become_create_entries_for_writes_only() {
        let fixture = Fixture::new();
        let targets = fixture
            .prepare("src/new/deeper/file.rs", FileMutationKind::Write)
            .unwrap();

        assert!(matches!(
            targets.traversal[..],
            [
                TraversalDirectory::Existing(_),
                TraversalDirectory::Create,
                TraversalDirectory::Create
            ]
        ));
        assert_eq!(targets.target_identity, None);
        assert!(!fixture.workspace.join("src/new").exists());
        assert_eq!(
            fixture.prepare("src/new/file.rs", FileMutationKind::Edit),
            Err(FileTargetFailure::Resolution("file_not_found"))
        );
        assert_eq!(
            fixture.prepare("src/missing.rs", FileMutationKind::Edit),
            Err(FileTargetFailure::Resolution("file_not_found"))
        );
    }

    #[test]
    fn final_symlinks_are_recorded_as_entries_and_escapes_are_refused() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root.join("outside")).unwrap();
        fs::write(fixture.root.join("outside/secret.txt"), "secret").unwrap();
        symlink(
            fixture.root.join("outside/secret.txt"),
            fixture.workspace.join("link.txt"),
        )
        .unwrap();
        symlink(fixture.root.join("outside"), fixture.workspace.join("dir")).unwrap();

        let link = fixture.prepare("link.txt", FileMutationKind::Edit).unwrap();
        assert_eq!(
            link.target_identity.map(FileIdentity::kind),
            Some(FileKind::Symlink)
        );
        assert_eq!(
            fixture.prepare("dir/secret.txt", FileMutationKind::Edit),
            Err(FileTargetFailure::Resolution("path_outside_workspace"))
        );
        let external = fixture.root.join("outside/secret.txt");
        let targets = fixture
            .prepare(external.to_str().unwrap(), FileMutationKind::Edit)
            .unwrap();
        assert!(targets.target.anchor_is_external);
        assert!(targets.traversal.is_empty());
    }

    #[test]
    fn resolution_failures_name_upstream_failure_tags() {
        let fixture = Fixture::new();
        for (path, tag) in [
            (".", "invalid_path"),
            ("src/main.rs/child", "not_directory"),
            ("~nobody/x", "invalid_path"),
        ] {
            assert_eq!(
                fixture.prepare(path, FileMutationKind::Write),
                Err(FileTargetFailure::Resolution(tag)),
                "{path}"
            );
        }
        assert_eq!(
            FileTargetFailure::from(PathError::SystemResources),
            FileTargetFailure::Operational(PathError::SystemResources)
        );
        assert_eq!(
            FileTargetFailure::from(PathError::ReadOnlyFileSystem),
            FileTargetFailure::Resolution("filesystem_unavailable")
        );
    }
}
