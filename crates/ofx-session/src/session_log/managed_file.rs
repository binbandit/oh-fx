use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::thread;
use std::time::{Duration, Instant};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RenameFlags, Stat};
use rustix::io::Errno;
use rustix::path::Arg;

use crate::session_error::SessionError;
use crate::session_layout::is_valid_session_id;

const LOCK_RETRY: Duration = Duration::from_millis(10);
const MAX_TREE_DEPTH: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    ReadOnly,
    Writable,
}

pub(crate) fn create_managed_file(dir: &PrivateDir, name: &str) -> Result<File, SessionError> {
    let fd = fs::openat(
        dir.as_fd(),
        name,
        OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        private_file_mode(),
    )
    .map_err(|errno| match errno {
        Errno::EXIST => SessionError::SessionAlreadyExists,
        Errno::LOOP | Errno::ISDIR | Errno::NOTDIR => SessionError::SessionPathUnsafe,
        other => other.into(),
    })?;
    fs::fchmod(&fd, private_file_mode())
        .map_err(|_| SessionError::PrivateStatePermissionsUnsupported)?;
    verify_managed_file(&fd, Access::Writable)?;
    Ok(File::from(fd))
}

pub(crate) fn open_managed_file(
    dir: &PrivateDir,
    name: &str,
    access: Access,
) -> Result<Option<File>, SessionError> {
    let mode = match access {
        Access::ReadOnly => OFlags::RDONLY,
        Access::Writable => OFlags::RDWR,
    };
    let flags = mode | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = match fs::openat(dir.as_fd(), name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::ISDIR | Errno::NOTDIR | Errno::NXIO) => {
            return Err(SessionError::SessionPathUnsafe);
        }
        Err(errno) => return Err(errno.into()),
    };
    verify_managed_file(&fd, access)?;
    Ok(Some(File::from(fd)))
}

pub(crate) fn read_managed_file(
    dir: &PrivateDir,
    name: &str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, SessionError> {
    let Some(file) = open_managed_file(dir, name, Access::ReadOnly)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    let limit = u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX);
    file.take(limit).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(SessionError::InvalidSessionFormat);
    }
    Ok(Some(bytes))
}

pub(crate) fn entry_exists(dir: &PrivateDir, name: &str) -> Result<bool, SessionError> {
    let stat = match fs::statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(false),
        Err(Errno::LOOP | Errno::NOTDIR) => return Err(SessionError::SessionPathUnsafe),
        Err(errno) => return Err(errno.into()),
    };
    match file_type(&stat) {
        FileType::Directory => Ok(true),
        FileType::RegularFile if stat.st_nlink == 1 => Ok(true),
        _ => Err(SessionError::SessionPathUnsafe),
    }
}

pub(crate) fn sync_dir(dir: &PrivateDir) -> Result<(), SessionError> {
    fs::fsync(dir.as_fd()).map_err(SessionError::from)
}

pub(crate) fn create_private_dir(parent: &PrivateDir, name: &str) -> Result<(), SessionError> {
    fs::mkdirat(parent.as_fd(), name, Mode::RWXU).map_err(SessionError::from)
}

pub(crate) fn publish_dir(
    parent: &PrivateDir,
    staging: &str,
    name: &str,
) -> Result<(), SessionError> {
    fs::renameat_with(
        parent.as_fd(),
        staging,
        parent.as_fd(),
        name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|errno| match errno {
        Errno::EXIST | Errno::NOTEMPTY => SessionError::SessionAlreadyExists,
        _ => SessionError::SessionStartFailed,
    })
}

pub(crate) fn remove_created_dir(parent: &PrivateDir, name: &str) {
    let _ = remove_tree(parent.as_fd(), name, MAX_TREE_DEPTH);
}

pub(crate) fn remove_session_dir(parent: &PrivateDir, name: &str) -> Result<(), SessionError> {
    remove_tree(parent.as_fd(), name, MAX_TREE_DEPTH)?;
    sync_dir(parent)
}

pub(crate) fn same_directory(
    first: &PrivateDir,
    second: &PrivateDir,
) -> Result<bool, SessionError> {
    let (first, second) = (fs::fstat(first.as_fd())?, fs::fstat(second.as_fd())?);
    Ok(first.st_dev == second.st_dev && first.st_ino == second.st_ino)
}

fn remove_tree<P: Arg + Copy>(parent: BorrowedFd<'_>, name: P, depth: usize) -> Result<(), Errno> {
    let dir = fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut names = Vec::new();
    for entry in fs::Dir::read_from(&dir)? {
        let entry = entry?;
        let entry_name = entry.file_name();
        if entry_name.to_bytes() == b"." || entry_name.to_bytes() == b".." {
            continue;
        }
        let directory = match entry.file_type() {
            FileType::Directory => true,
            FileType::Unknown => fs::statat(&dir, entry_name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|stat| file_type(&stat) == FileType::Directory),
            _ => false,
        };
        names.push((entry_name.to_owned(), directory));
    }
    for (entry_name, directory) in names {
        if directory {
            let depth = depth.checked_sub(1).ok_or(Errno::NOTEMPTY)?;
            remove_tree(dir.as_fd(), entry_name.as_c_str(), depth)?;
        } else {
            fs::unlinkat(&dir, entry_name.as_c_str(), AtFlags::empty())?;
        }
    }
    fs::unlinkat(parent, name, AtFlags::REMOVEDIR)
}

pub(crate) fn session_directory_names(dir: &PrivateDir) -> Result<Vec<String>, SessionError> {
    directory_names(dir, is_valid_session_id)
}

pub(crate) fn directory_names(
    dir: &PrivateDir,
    wanted: impl Fn(&str) -> bool,
) -> Result<Vec<String>, SessionError> {
    let mut names = Vec::new();
    for entry in fs::Dir::read_from(dir.as_fd())? {
        let entry = entry?;
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if !wanted(name) {
            continue;
        }
        let directory = match entry.file_type() {
            FileType::Directory => true,
            FileType::Unknown => fs::statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|stat| file_type(&stat) == FileType::Directory),
            _ => false,
        };
        if directory {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

pub(crate) fn lock_with_deadline(
    dir: &PrivateDir,
    name: &str,
    deadline: Duration,
) -> Result<Option<AdvisoryLock>, DurableError> {
    let started = Instant::now();
    loop {
        if let Some(lock) = dir.try_lock(name)? {
            return Ok(Some(lock));
        }
        if started.elapsed() >= deadline {
            return Ok(None);
        }
        thread::sleep(LOCK_RETRY.min(deadline));
    }
}

pub(crate) fn has_private_dir_mode(dir: &PrivateDir) -> Result<bool, SessionError> {
    let stat = fs::fstat(dir.as_fd())?;
    Ok(permissions(&stat) == Mode::RWXU)
}

pub(crate) fn file_type(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

pub(crate) fn permissions(stat: &Stat) -> Mode {
    Mode::from_raw_mode(stat.st_mode) & (Mode::RWXU | Mode::RWXG | Mode::RWXO)
}

pub(crate) fn private_file_mode() -> Mode {
    Mode::RUSR | Mode::WUSR
}

fn verify_managed_file(fd: &OwnedFd, access: Access) -> Result<(), SessionError> {
    let stat = fs::fstat(fd)?;
    if file_type(&stat) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(SessionError::SessionPathUnsafe);
    }
    if access == Access::Writable && permissions(&stat) != private_file_mode() {
        return Err(SessionError::PrivateStatePermissionsUnsupported);
    }
    Ok(())
}
