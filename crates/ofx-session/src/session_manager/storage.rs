use std::fmt;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

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

pub(crate) fn read_file(dir: impl AsFd, name: &str) -> Result<Vec<u8>, StorageError> {
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = fs::openat(dir, name, flags, Mode::empty())?;
    if FileType::from_raw_mode(fs::fstat(&fd)?.st_mode) != FileType::RegularFile {
        return Err(StorageError::Fault(IoFault::AccessDenied));
    }
    let mut bytes = Vec::new();
    File::from(fd).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn is_directory(dir: impl AsFd, name: &str) -> Result<bool, IoFault> {
    match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(FileType::from_raw_mode(stat.st_mode) == FileType::Directory),
        Err(Errno::NOENT) => Ok(false),
        Err(errno) => Err(fault(errno)),
    }
}
