use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use ofx_text::lowercase_hex;
use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags, Stat};
use rustix::io::Errno;
use zeroize::Zeroizing;

const TEMP_SUFFIX_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DurableError {
    #[error("DurablePathUnsafe")]
    PathUnsafe,
    #[error("PrivateStatePermissionsUnsupported")]
    PermissionsUnsupported,
    #[error("DurableReplacePreRenameFailed")]
    PreRenameFailed,
    #[error("DurableReplacePostRenameFailed")]
    PostRenameFailed,
    #[error("AccessDenied")]
    AccessDenied,
    #[error("LockUnsupported")]
    LockUnsupported,
    #[error("InsecureAuthFile")]
    InsecureFile,
    #[error("FileTooBig")]
    TooLarge,
    #[error("StorageFailed")]
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    Missing,
    RemovedNotDurable,
}

#[derive(Debug)]
pub struct AdvisoryLock {
    _file: OwnedFd,
}

#[derive(Debug)]
pub struct PrivateDir {
    fd: OwnedFd,
}

impl PrivateDir {
    pub fn open_existing(path: &Path) -> Result<Option<Self>, DurableError> {
        let (parent, leaf) = split(path)?;
        let parent = match open_directory(parent) {
            Ok(parent) => parent,
            Err(Errno::NOENT) => return Ok(None),
            Err(_) => return Err(DurableError::Failed),
        };
        let fd = match fs::openat(&parent, leaf, directory_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(leaf_error(error)),
        };
        let stat = fs::fstat(&fd).map_err(|_| DurableError::Failed)?;
        if file_type(&stat) != FileType::Directory {
            return Err(DurableError::PathUnsafe);
        }
        Ok(Some(Self { fd }))
    }

    pub fn open_existing_private(path: &Path) -> Result<Option<Self>, DurableError> {
        let Some(directory) = Self::open_existing(path)? else {
            return Ok(None);
        };
        if !directory.owner_writable() {
            return Err(DurableError::PermissionsUnsupported);
        }
        make_private_directory(&directory.fd)?;
        Ok(Some(directory))
    }

    pub fn open_or_create(path: &Path) -> Result<Self, DurableError> {
        let (parent_path, leaf) = split(path)?;
        std::fs::create_dir_all(parent_path).map_err(|_| DurableError::Failed)?;
        let parent = open_directory(parent_path).map_err(|_| DurableError::Failed)?;
        open_or_create_in(&parent, leaf)
    }

    pub fn open_or_create_child(&self, name: &str) -> Result<Self, DurableError> {
        open_or_create_in(&self.fd, name)
    }

    pub fn open_child(&self, name: &str) -> Result<Option<Self>, DurableError> {
        let fd = match fs::openat(&self.fd, name, directory_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(leaf_error(error)),
        };
        let stat = fs::fstat(&fd).map_err(|_| DurableError::Failed)?;
        if file_type(&stat) != FileType::Directory {
            return Err(DurableError::PathUnsafe);
        }
        Ok(Some(Self { fd }))
    }

    pub fn open_child_private(&self, name: &str) -> Result<Option<Self>, DurableError> {
        let Some(directory) = self.open_child(name)? else {
            return Ok(None);
        };
        make_private_directory(&directory.fd)?;
        Ok(Some(directory))
    }

    pub fn ensure_private(&self) -> Result<(), DurableError> {
        make_private_directory(&self.fd)
    }

    pub fn owner_writable(&self) -> bool {
        fs::fstat(&self.fd).is_ok_and(|stat| permissions(&stat).contains(Mode::WUSR))
    }

    pub fn private_file_exists(&self, name: &str) -> Result<bool, DurableError> {
        let stat = match fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(false),
            Err(_) => return Err(DurableError::Failed),
        };
        if file_type(&stat) != FileType::RegularFile
            || stat.st_nlink != 1
            || permissions(&stat) != private_file_mode()
        {
            return Err(DurableError::PathUnsafe);
        }
        Ok(true)
    }

    pub fn read_private(
        &self,
        name: &str,
        max_bytes: usize,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, DurableError> {
        let fd = match fs::openat(&self.fd, name, read_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(_) => return Err(DurableError::Failed),
        };
        let stat = fs::fstat(&fd).map_err(|_| DurableError::Failed)?;
        if file_type(&stat) != FileType::RegularFile
            || stat.st_nlink != 1
            || permissions(&stat).intersects(Mode::RWXG | Mode::RWXO)
        {
            return Err(DurableError::InsecureFile);
        }
        let size = usize::try_from(stat.st_size).map_err(|_| DurableError::TooLarge)?;
        if size > max_bytes {
            return Err(DurableError::TooLarge);
        }
        let mut bytes = Zeroizing::new(Vec::with_capacity(size + 1));
        let limit = u64::try_from(max_bytes + 1).map_err(|_| DurableError::TooLarge)?;
        File::from(fd)
            .take(limit)
            .read_to_end(&mut bytes)
            .map_err(|_| DurableError::Failed)?;
        if bytes.len() > max_bytes {
            return Err(DurableError::TooLarge);
        }
        Ok(Some(bytes))
    }

    pub fn private_file_present(&self, name: &str, max_bytes: usize) -> Option<bool> {
        let fd = match fs::openat(&self.fd, name, read_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Some(false),
            Err(_) => return None,
        };
        let stat = fs::fstat(&fd).ok()?;
        let size = usize::try_from(stat.st_size).ok()?;
        let usable = file_type(&stat) == FileType::RegularFile
            && stat.st_nlink == 1
            && !permissions(&stat).intersects(Mode::RWXG | Mode::RWXO)
            && size > 0
            && size <= max_bytes;
        usable.then_some(true)
    }

    pub fn replace(&self, name: &str, bytes: &[u8]) -> Result<(), DurableError> {
        self.validate_replace_target(name)?;
        let temp = temp_name(name)?;
        let file = fs::openat(
            &self.fd,
            temp.as_str(),
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            private_file_mode(),
        )
        .map_err(|_| DurableError::PreRenameFailed)?;
        if let Err(error) = self.fill_and_rename(file, &temp, name, bytes) {
            self.remove_temp(&temp);
            return Err(error);
        }
        let stat = fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| DurableError::PostRenameFailed)?;
        if file_type(&stat) != FileType::RegularFile
            || stat.st_nlink != 1
            || permissions(&stat) != private_file_mode()
        {
            return Err(DurableError::PostRenameFailed);
        }
        fs::fsync(&self.fd).map_err(|_| DurableError::PostRenameFailed)
    }

    pub fn remove(&self, name: &str) -> Result<RemoveOutcome, DurableError> {
        match fs::unlinkat(&self.fd, name, AtFlags::empty()) {
            Ok(()) => {}
            Err(Errno::NOENT) => return Ok(RemoveOutcome::Missing),
            Err(_) => return Err(DurableError::Failed),
        }
        Ok(if fs::fsync(&self.fd).is_ok() {
            RemoveOutcome::Removed
        } else {
            RemoveOutcome::RemovedNotDurable
        })
    }

    pub(crate) fn read_owned(
        &self,
        name: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, DurableError> {
        let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY;
        let fd = match fs::openat(&self.fd, name, flags | OFlags::NONBLOCK, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP | Errno::ISDIR | Errno::NOTDIR | Errno::NXIO) => {
                return Err(DurableError::PathUnsafe);
            }
            Err(Errno::ACCESS | Errno::PERM | Errno::ROFS) => {
                return Err(DurableError::AccessDenied);
            }
            Err(_) => return Err(DurableError::Failed),
        };
        let stat = fs::fstat(&fd).map_err(|_| DurableError::Failed)?;
        if file_type(&stat) != FileType::RegularFile || stat.st_nlink != 1 {
            return Err(DurableError::PathUnsafe);
        }
        fs::fchmod(&fd, private_file_mode()).map_err(|_| DurableError::PermissionsUnsupported)?;
        let stat = fs::fstat(&fd).map_err(|_| DurableError::Failed)?;
        if permissions(&stat) != private_file_mode() {
            return Err(DurableError::PermissionsUnsupported);
        }
        read_limited(fd, &stat, max_bytes).map(Some)
    }

    pub(crate) fn names(&self) -> Result<Vec<String>, DurableError> {
        let directory = fs::Dir::read_from(&self.fd).map_err(|_| DurableError::Failed)?;
        let mut names = Vec::new();
        for entry in directory {
            let entry = entry.map_err(|_| DurableError::Failed)?;
            if let Ok(name) = entry.file_name().to_str()
                && name != "."
                && name != ".."
            {
                names.push(name.to_owned());
            }
        }
        Ok(names)
    }

    pub(crate) fn is_single_link_file(&self, name: &str) -> bool {
        fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| file_type(&stat) == FileType::RegularFile && stat.st_nlink == 1)
    }

    pub(crate) fn read_single_link_file(&self, name: &str, max_bytes: usize) -> Option<Vec<u8>> {
        let fd = fs::openat(&self.fd, name, read_flags(), Mode::empty()).ok()?;
        let stat = fs::fstat(&fd).ok()?;
        if file_type(&stat) != FileType::RegularFile || stat.st_nlink != 1 {
            return None;
        }
        read_limited(fd, &stat, max_bytes).ok()
    }

    pub fn try_lock(&self, name: &str) -> Result<Option<AdvisoryLock>, DurableError> {
        let file = self.open_lock_file(name)?;
        match fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(AdvisoryLock { _file: file })),
            Err(Errno::WOULDBLOCK | Errno::INTR) => Ok(None),
            Err(Errno::NOLCK | Errno::OPNOTSUPP) => Err(DurableError::LockUnsupported),
            Err(_) => Err(DurableError::Failed),
        }
    }

    fn validate_replace_target(&self, name: &str) -> Result<(), DurableError> {
        let stat = match fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(()),
            Err(Errno::LOOP | Errno::NOTDIR) => return Err(DurableError::PathUnsafe),
            Err(_) => return Err(DurableError::PreRenameFailed),
        };
        if file_type(&stat) != FileType::RegularFile || stat.st_nlink != 1 {
            return Err(DurableError::PathUnsafe);
        }
        if !permissions(&stat).intersects(Mode::WUSR | Mode::WGRP | Mode::WOTH) {
            return Err(DurableError::AccessDenied);
        }
        Ok(())
    }

    fn fill_and_rename(
        &self,
        file: OwnedFd,
        temp: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), DurableError> {
        fs::fchmod(&file, private_file_mode()).map_err(|_| DurableError::PermissionsUnsupported)?;
        let stat = fs::fstat(&file).map_err(|_| DurableError::PreRenameFailed)?;
        if file_type(&stat) != FileType::RegularFile || stat.st_nlink != 1 {
            return Err(DurableError::PathUnsafe);
        }
        if permissions(&stat) != private_file_mode() {
            return Err(DurableError::PermissionsUnsupported);
        }
        let mut file = File::from(file);
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| DurableError::PreRenameFailed)?;
        fs::renameat(&self.fd, temp, &self.fd, name).map_err(|_| DurableError::PreRenameFailed)
    }

    fn remove_temp(&self, temp: &str) {
        let Ok(stat) = fs::statat(&self.fd, temp, AtFlags::SYMLINK_NOFOLLOW) else {
            return;
        };
        if file_type(&stat) == FileType::RegularFile && stat.st_nlink == 1 {
            let _ = fs::unlinkat(&self.fd, temp, AtFlags::empty());
        }
    }

    fn open_lock_file(&self, name: &str) -> Result<OwnedFd, DurableError> {
        let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        let file = match fs::openat(&self.fd, name, flags, Mode::empty()) {
            Ok(file) => file,
            Err(Errno::NOENT) => {
                match fs::openat(
                    &self.fd,
                    name,
                    flags | OFlags::CREATE | OFlags::EXCL,
                    private_file_mode(),
                ) {
                    Ok(created) => {
                        fs::fchmod(&created, private_file_mode())
                            .map_err(|_| DurableError::PermissionsUnsupported)?;
                        fs::fsync(&self.fd).map_err(|_| DurableError::Failed)?;
                        created
                    }
                    Err(Errno::EXIST) => fs::openat(&self.fd, name, flags, Mode::empty())
                        .map_err(|_| DurableError::Failed)?,
                    Err(_) => return Err(DurableError::Failed),
                }
            }
            Err(Errno::LOOP | Errno::ISDIR | Errno::NOTDIR) => {
                return Err(DurableError::PathUnsafe);
            }
            Err(_) => return Err(DurableError::Failed),
        };
        let stat = fs::fstat(&file).map_err(|_| DurableError::Failed)?;
        if file_type(&stat) != FileType::RegularFile || stat.st_nlink != 1 {
            return Err(DurableError::PathUnsafe);
        }
        if permissions(&stat) != private_file_mode() {
            return Err(DurableError::PermissionsUnsupported);
        }
        Ok(file)
    }
}

impl AsFd for PrivateDir {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

fn split(path: &Path) -> Result<(&Path, &std::ffi::OsStr), DurableError> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(leaf)) if !parent.as_os_str().is_empty() => Ok((parent, leaf)),
        _ => Err(DurableError::PathUnsafe),
    }
}

fn open_or_create_in<P: rustix::path::Arg + Copy>(
    parent: &OwnedFd,
    leaf: P,
) -> Result<PrivateDir, DurableError> {
    let mut created = false;
    let fd = match fs::openat(parent, leaf, directory_flags(), Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => {
            match fs::mkdirat(parent, leaf, Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(_) => return Err(DurableError::Failed),
            }
            created = true;
            fs::openat(parent, leaf, directory_flags(), Mode::empty()).map_err(leaf_error)?
        }
        Err(error) => return Err(leaf_error(error)),
    };
    make_private_directory(&fd)?;
    if created {
        fs::fsync(parent).map_err(|_| DurableError::Failed)?;
    }
    Ok(PrivateDir { fd })
}

fn open_directory(path: &Path) -> Result<OwnedFd, Errno> {
    fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn read_limited(fd: OwnedFd, stat: &Stat, max_bytes: usize) -> Result<Vec<u8>, DurableError> {
    if usize::try_from(stat.st_size).map_or(true, |size| size > max_bytes) {
        return Err(DurableError::TooLarge);
    }
    let limit = u64::try_from(max_bytes + 1).map_err(|_| DurableError::TooLarge)?;
    let mut bytes = Vec::new();
    File::from(fd)
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|_| DurableError::Failed)?;
    if bytes.len() > max_bytes {
        return Err(DurableError::TooLarge);
    }
    Ok(bytes)
}

fn read_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK
}

fn leaf_error(error: Errno) -> DurableError {
    match error {
        Errno::LOOP | Errno::NOTDIR => DurableError::PathUnsafe,
        _ => DurableError::Failed,
    }
}

fn make_private_directory(fd: &OwnedFd) -> Result<(), DurableError> {
    fs::fchmod(fd, Mode::RWXU).map_err(|_| DurableError::PermissionsUnsupported)?;
    let stat = fs::fstat(fd).map_err(|_| DurableError::Failed)?;
    if file_type(&stat) != FileType::Directory {
        return Err(DurableError::PathUnsafe);
    }
    if permissions(&stat) != Mode::RWXU {
        return Err(DurableError::PermissionsUnsupported);
    }
    Ok(())
}

fn private_file_mode() -> Mode {
    Mode::RUSR | Mode::WUSR
}

fn file_type(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

fn permissions(stat: &Stat) -> Mode {
    Mode::from_raw_mode(stat.st_mode) & (Mode::RWXU | Mode::RWXG | Mode::RWXO)
}

fn temp_name(name: &str) -> Result<String, DurableError> {
    let mut suffix = [0_u8; TEMP_SUFFIX_BYTES];
    getrandom::fill(&mut suffix).map_err(|_| DurableError::PreRenameFailed)?;
    Ok(format!(".{name}.tmp.{}", lowercase_hex(&suffix)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn private_directories_are_created_with_owner_only_access() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data/oh-fx");
        let directory = PrivateDir::open_or_create(&path).unwrap();
        assert_eq!(mode(&path), 0o700);
        assert!(directory.owner_writable());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        PrivateDir::open_existing(&path).unwrap().unwrap();
        assert_eq!(mode(&path), 0o755);
        PrivateDir::open_existing_private(&path).unwrap().unwrap();
        assert_eq!(mode(&path), 0o700);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
        directory.ensure_private().unwrap();
        assert_eq!(mode(&path), 0o700);
        assert!(
            PrivateDir::open_existing(&root.path().join("missing/oh-fx"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn symlinked_private_directories_are_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("elsewhere")).unwrap();
        symlink(root.path().join("elsewhere"), root.path().join("oh-fx")).unwrap();
        assert_eq!(
            PrivateDir::open_existing(&root.path().join("oh-fx")).unwrap_err(),
            DurableError::PathUnsafe
        );
        assert_eq!(
            PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap_err(),
            DurableError::PathUnsafe
        );
    }

    #[test]
    fn child_directories_open_without_following_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let parent = PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap();
        assert!(parent.open_child("missing").unwrap().is_none());
        assert!(parent.open_child_private("missing").unwrap().is_none());
        let child = root.path().join("oh-fx/child");
        std::fs::create_dir(&child).unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
        parent.open_child("child").unwrap().unwrap();
        assert_eq!(mode(&child), 0o755);
        let opened = parent.open_child_private("child").unwrap().unwrap();
        assert_eq!(mode(&child), 0o700);
        assert!(fs::fstat(opened.as_fd()).is_ok());
        symlink(&child, root.path().join("oh-fx/linked")).unwrap();
        std::fs::write(root.path().join("oh-fx/file"), "").unwrap();
        for name in ["linked", "file"] {
            assert_eq!(
                parent.open_child(name).unwrap_err(),
                DurableError::PathUnsafe,
                "{name}"
            );
        }
    }

    #[test]
    fn durable_replace_writes_private_files_without_leaving_temporaries() {
        let root = tempfile::tempdir().unwrap();
        let directory = PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap();
        directory.replace("auth.json", b"first").unwrap();
        directory.replace("auth.json", b"second").unwrap();
        let file = root.path().join("oh-fx/auth.json");
        assert_eq!(std::fs::read(&file).unwrap(), b"second");
        assert_eq!(mode(&file), 0o600);
        let names: Vec<_> = std::fs::read_dir(root.path().join("oh-fx"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["auth.json"]);
        assert_eq!(
            directory
                .read_private("auth.json", 64)
                .unwrap()
                .unwrap()
                .as_slice(),
            b"second"
        );
        assert_eq!(
            directory.read_private("auth.json", 3),
            Err(DurableError::TooLarge)
        );
        let read = directory
            .read_private("auth.json", 64 * 1024 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(read.as_slice(), b"second");
        assert!(read.capacity() < 4096, "{}", read.capacity());
    }

    #[test]
    fn durable_replace_refuses_links_and_read_only_targets() {
        let root = tempfile::tempdir().unwrap();
        let directory = PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, "keep").unwrap();
        symlink(&outside, root.path().join("oh-fx/linked.json")).unwrap();
        assert_eq!(
            directory.replace("linked.json", b"x"),
            Err(DurableError::PathUnsafe)
        );
        std::fs::hard_link(&outside, root.path().join("oh-fx/hard.json")).unwrap();
        assert_eq!(
            directory.replace("hard.json", b"x"),
            Err(DurableError::PathUnsafe)
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        directory.replace("locked.json", b"x").unwrap();
        std::fs::set_permissions(
            root.path().join("oh-fx/locked.json"),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        assert_eq!(
            directory.replace("locked.json", b"y"),
            Err(DurableError::AccessDenied)
        );
    }

    #[test]
    fn reads_refuse_shared_and_linked_files() {
        let root = tempfile::tempdir().unwrap();
        let directory = PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap();
        let shared = root.path().join("oh-fx/shared.json");
        std::fs::write(&shared, "{}").unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            directory.read_private("shared.json", 64),
            Err(DurableError::InsecureFile)
        );
        assert_eq!(directory.private_file_present("shared.json", 64), None);
        symlink(&shared, root.path().join("oh-fx/link.json")).unwrap();
        assert_eq!(
            directory.read_private("link.json", 64),
            Err(DurableError::Failed)
        );
        assert_eq!(directory.read_private("missing.json", 64), Ok(None));
        assert_eq!(
            directory.private_file_present("missing.json", 64),
            Some(false)
        );
    }

    #[test]
    fn removal_reports_missing_files() {
        let root = tempfile::tempdir().unwrap();
        let directory = PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap();
        directory.replace("auth.json", b"x").unwrap();
        assert_eq!(directory.remove("auth.json"), Ok(RemoveOutcome::Removed));
        assert_eq!(directory.remove("auth.json"), Ok(RemoveOutcome::Missing));
    }

    #[test]
    fn advisory_locks_exclude_a_second_holder_until_released() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("oh-fx");
        let first = PrivateDir::open_or_create(&path).unwrap();
        let second = PrivateDir::open_existing(&path).unwrap().unwrap();
        let held = first.try_lock("auth.lock").unwrap().unwrap();
        assert!(second.try_lock("auth.lock").unwrap().is_none());
        drop(held);
        assert!(second.try_lock("auth.lock").unwrap().is_some());
        assert_eq!(mode(&path.join("auth.lock")), 0o600);
    }
}
