use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};

use rustix::fs::{AtFlags, FileType, Mode, Stat, fstat, openat, statat};
use rustix::io::Errno;

use crate::path_error::PathError;
use crate::regular_file::DIRECTORY_FLAGS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    device: i128,
    inode: u64,
    kind: FileKind,
}

impl FileIdentity {
    fn of(stat: &Stat) -> Self {
        let kind = match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => FileKind::Directory,
            FileType::RegularFile => FileKind::RegularFile,
            FileType::Symlink => FileKind::Symlink,
            _ => FileKind::Other,
        };
        Self {
            device: i128::from(stat.st_dev),
            inode: stat.st_ino,
            kind,
        }
    }

    pub fn kind(self) -> FileKind {
        self.kind
    }
}

pub fn descriptor_identity(descriptor: impl AsFd) -> Result<FileIdentity, PathError> {
    fstat(descriptor)
        .map(|stat| FileIdentity::of(&stat))
        .map_err(errno_error)
}

pub fn entry_identity(directory: impl AsFd, name: &OsStr) -> Result<FileIdentity, PathError> {
    statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| FileIdentity::of(&stat))
        .map_err(errno_error)
}

pub fn open_directory(path: &Path) -> Result<OwnedFd, PathError> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(PathError::InvalidPath);
    }
    let root = rustix::fs::open("/", DIRECTORY_FLAGS, Mode::empty()).map_err(errno_error)?;
    components.try_fold(root, |directory, component| match component {
        Component::Normal(name) => open_child_directory(&directory, name),
        _ => Err(PathError::InvalidPath),
    })
}

pub fn open_child_directory(directory: impl AsFd, name: &OsStr) -> Result<OwnedFd, PathError> {
    openat(directory, name, DIRECTORY_FLAGS, Mode::empty()).map_err(errno_error)
}

fn errno_error(errno: Errno) -> PathError {
    io::Error::from(errno).into()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    fn fixture() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/file.txt"), "x").unwrap();
        symlink(root.join("a/b"), root.join("a/link")).unwrap();
        (temp, root)
    }

    #[test]
    fn identities_follow_the_inode_and_never_the_symlink_target() {
        let (_temp, root) = fixture();
        let directory = open_directory(&root.join("a")).unwrap();
        let child = open_child_directory(&directory, OsStr::new("b")).unwrap();

        assert_eq!(
            descriptor_identity(&child).unwrap(),
            entry_identity(&directory, OsStr::new("b")).unwrap()
        );
        assert_eq!(
            entry_identity(&directory, OsStr::new("b")).unwrap().kind(),
            FileKind::Directory
        );
        assert_eq!(
            entry_identity(&directory, OsStr::new("file.txt"))
                .unwrap()
                .kind(),
            FileKind::RegularFile
        );
        assert_eq!(
            entry_identity(&directory, OsStr::new("link"))
                .unwrap()
                .kind(),
            FileKind::Symlink
        );
        assert_ne!(
            entry_identity(&directory, OsStr::new("link")).unwrap(),
            descriptor_identity(&child).unwrap()
        );
    }

    #[test]
    fn directories_open_without_following_symlinks_in_any_component() {
        let (_temp, root) = fixture();
        assert!(open_directory(&root.join("a/link")).is_err());
        assert!(open_directory(&root.join("a/link/..")).is_err());
        assert_eq!(
            open_directory(Path::new("relative")).err(),
            Some(PathError::InvalidPath)
        );
        let directory = open_directory(&root.join("a")).unwrap();
        assert!(open_child_directory(&directory, OsStr::new("link")).is_err());
        assert_eq!(
            open_child_directory(&directory, OsStr::new("missing")).err(),
            Some(PathError::FileNotFound)
        );
        assert_eq!(
            open_child_directory(&directory, OsStr::new("file.txt")).err(),
            Some(PathError::NotDir)
        );
    }
}
