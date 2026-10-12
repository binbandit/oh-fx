use std::fmt;
use std::fs::{DirBuilder, File};
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use rustix::path::Arg;

const FOLDER_MODE: u32 = 0o700;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IoFault {
    Io,
    NoSpaceLeft,
    AccessDenied,
    ReadOnlyFileSystem,
    FileTooBig,
}

impl fmt::Display for IoFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io => "Io",
            Self::NoSpaceLeft => "NoSpaceLeft",
            Self::AccessDenied => "AccessDenied",
            Self::ReadOnlyFileSystem => "ReadOnlyFileSystem",
            Self::FileTooBig => "FileTooBig",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorageError {
    NotFound,
    Fault(IoFault),
}

impl From<Errno> for StorageError {
    fn from(errno: Errno) -> Self {
        match errno {
            Errno::NOENT => Self::NotFound,
            other => Self::Fault(fault(other)),
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(error: std::io::Error) -> Self {
        error
            .raw_os_error()
            .map_or(Self::Fault(IoFault::Io), |code| {
                Errno::from_raw_os_error(code).into()
            })
    }
}

pub(crate) fn io_fault(error: std::io::Error) -> IoFault {
    match StorageError::from(error) {
        StorageError::Fault(fault) => fault,
        StorageError::NotFound => IoFault::Io,
    }
}

fn fault(errno: Errno) -> IoFault {
    match errno {
        Errno::LOOP | Errno::NOTDIR | Errno::ISDIR | Errno::ACCESS | Errno::PERM | Errno::ILSEQ => {
            IoFault::AccessDenied
        }
        Errno::NOSPC | Errno::DQUOT => IoFault::NoSpaceLeft,
        Errno::ROFS => IoFault::ReadOnlyFileSystem,
        Errno::FBIG => IoFault::FileTooBig,
        _ => IoFault::Io,
    }
}

pub(crate) fn open_file(dir: impl AsFd, name: &str) -> Result<File, StorageError> {
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = fs::openat(dir, name, flags, Mode::empty())?;
    if FileType::from_raw_mode(fs::fstat(&fd)?.st_mode) != FileType::RegularFile {
        return Err(StorageError::Fault(IoFault::AccessDenied));
    }
    Ok(File::from(fd))
}

pub(crate) fn read_file(dir: impl AsFd, name: &str) -> Result<Vec<u8>, StorageError> {
    let mut bytes = Vec::new();
    open_file(dir, name)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn open_dir(dir: impl AsFd, name: &str) -> Result<OwnedFd, StorageError> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(fs::openat(dir, name, flags, Mode::empty())?)
}

pub(crate) fn is_directory(dir: impl AsFd, name: &str) -> Result<bool, IoFault> {
    match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(FileType::from_raw_mode(stat.st_mode) == FileType::Directory),
        Err(Errno::NOENT) => Ok(false),
        Err(errno) => Err(fault(errno)),
    }
}

pub(crate) fn open_root(path: &Path) -> Result<OwnedFd, IoFault> {
    let (Some(parent), Some(leaf)) = (path.parent(), path.file_name()) else {
        return Err(IoFault::Io);
    };
    DirBuilder::new()
        .recursive(true)
        .mode(FOLDER_MODE)
        .create(parent)
        .map_err(io_fault)?;
    let parent = fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(fault)?;
    ensure_dir(&parent, leaf)
}

pub(crate) fn ensure_dir<P: Arg + Copy>(parent: impl AsFd, name: P) -> Result<OwnedFd, IoFault> {
    match fs::mkdirat(&parent, name, Mode::RWXU) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(errno) => return Err(fault(errno)),
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    fs::openat(parent, name, flags, Mode::empty()).map_err(fault)
}

pub(crate) fn make_dir(parent: impl AsFd, name: &str) -> Result<(), IoFault> {
    fs::mkdirat(parent, name, Mode::RWXU).map_err(fault)
}

pub(crate) fn create_file(dir: impl AsFd, name: &str, mode: Mode) -> Result<File, IoFault> {
    let flags = OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(File::from(
        fs::openat(dir, name, flags, mode).map_err(fault)?,
    ))
}

pub(crate) fn open_writable(dir: impl AsFd, name: &str) -> Result<Option<File>, IoFault> {
    let flags =
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = match fs::openat(dir, name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(errno) => return Err(fault(errno)),
    };
    if FileType::from_raw_mode(fs::fstat(&fd).map_err(fault)?.st_mode) != FileType::RegularFile {
        return Err(IoFault::AccessDenied);
    }
    Ok(Some(File::from(fd)))
}

pub(crate) fn exists(dir: impl AsFd, name: &str) -> bool {
    fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).is_ok()
}

pub(crate) fn rename(dir: impl AsFd, from: &str, to: &str) -> Result<(), IoFault> {
    fs::renameat(&dir, from, &dir, to).map_err(fault)
}

pub(crate) fn rename_into(
    from_dir: impl AsFd,
    name: &str,
    to_dir: impl AsFd,
) -> Result<(), IoFault> {
    fs::renameat(from_dir, name, to_dir, name).map_err(fault)
}

pub(crate) fn remove(dir: impl AsFd, name: &str) {
    let _ = fs::unlinkat(dir, name, AtFlags::empty());
}

pub(crate) fn sync_dir(dir: impl AsFd) -> Result<(), IoFault> {
    fs::fsync(dir).map_err(fault)
}

pub(crate) fn try_lock(file: &File) -> Result<bool, IoFault> {
    match fs::flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(Errno::WOULDBLOCK | Errno::INTR) => Ok(false),
        Err(errno) => Err(fault(errno)),
    }
}

pub(crate) fn unlock(file: &File) {
    let _ = fs::flock(file, FlockOperation::Unlock);
}
