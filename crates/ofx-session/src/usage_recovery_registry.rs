use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

use ofx_config::{PrivateDir, RemoveOutcome};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::session_error::SessionError;
use crate::session_layout::is_valid_session_id;
use crate::session_log::managed_file::{file_type, permissions, private_file_mode};

pub(crate) const USAGE_RECOVERY_DIR: &str = "usage-recovery";
const MARKER_PREFIX: &str = "v1 ";
const MAX_MARKER_BYTES: usize = MARKER_PREFIX.len() + 20 + 1;
const MAX_MARKED_SESSIONS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkedSession {
    pub(crate) id: String,
    pub(crate) protected_updated_at_ms: i64,
    pub(crate) marker_modified_at_ns: i128,
}

pub(crate) struct RecoveryRegistry {
    data: PrivateDir,
}

impl RecoveryRegistry {
    pub(crate) fn new(data: PrivateDir) -> Self {
        Self { data }
    }

    pub(crate) fn mark(
        &self,
        id: &str,
        protected_updated_at_ms: i64,
        replace_existing: bool,
    ) -> Result<(), SessionError> {
        if !is_valid_session_id(id) || protected_updated_at_ms < 0 {
            return Err(SessionError::InvalidUsageRecoveryIndex);
        }
        let recovery = self.data.open_or_create_child(USAGE_RECOVERY_DIR)?;
        let existing = marker(&recovery, id)?.map(|(protected, _)| protected);
        if (existing.is_some() && !replace_existing) || existing == Some(protected_updated_at_ms) {
            return Ok(());
        }
        let bytes = format!("{MARKER_PREFIX}{protected_updated_at_ms}\n");
        Ok(recovery.replace(id, bytes.as_bytes())?)
    }

    pub(crate) fn clear(&self, id: &str) -> Result<(), SessionError> {
        if !is_valid_session_id(id) {
            return Err(SessionError::InvalidUsageRecoveryIndex);
        }
        let Some(recovery) = recovery_dir(&self.data)? else {
            return Ok(());
        };
        if marker(&recovery, id)?.is_none() {
            return Ok(());
        }
        match recovery.remove(id)? {
            RemoveOutcome::Removed | RemoveOutcome::Missing => Ok(()),
            RemoveOutcome::RemovedNotDurable => Err(SessionError::InvalidUsageRecoveryIndex),
        }
    }
}

pub(crate) fn marked_sessions(data: &PrivateDir) -> Result<Vec<MarkedSession>, SessionError> {
    let Some(recovery) = recovery_dir(data)? else {
        return Ok(Vec::new());
    };
    let invalid = SessionError::InvalidUsageRecoveryIndex;
    let entries = fs::Dir::read_from(recovery.as_fd()).map_err(|_| invalid)?;
    let mut marked = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| invalid)?;
        let name = entry.file_name().to_str().map_err(|_| invalid)?;
        if name == "." || name == ".." {
            continue;
        }
        if marked.len() == MAX_MARKED_SESSIONS || !is_valid_session_id(name) {
            return Err(invalid);
        }
        let (protected_updated_at_ms, stat) = marker(&recovery, name)?.ok_or(invalid)?;
        marked.push(MarkedSession {
            id: name.to_owned(),
            protected_updated_at_ms,
            marker_modified_at_ns: modified_at_ns(&stat),
        });
    }
    marked.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(marked)
}

pub(crate) fn modified_at_ns(stat: &Stat) -> i128 {
    i128::from(stat.st_mtime) * 1_000_000_000 + i128::from(stat.st_mtime_nsec)
}

fn recovery_dir(data: &PrivateDir) -> Result<Option<PrivateDir>, SessionError> {
    let recovery = match data.open_child(USAGE_RECOVERY_DIR) {
        Ok(Some(recovery)) => recovery,
        Ok(None) => return Ok(None),
        Err(_) => return Err(SessionError::InvalidUsageRecoveryIndex),
    };
    let stat = fs::fstat(recovery.as_fd())?;
    if file_type(&stat) != FileType::Directory || permissions(&stat) != Mode::RWXU {
        return Err(SessionError::InvalidUsageRecoveryIndex);
    }
    Ok(Some(recovery))
}

fn marker(recovery: &PrivateDir, id: &str) -> Result<Option<(i64, Stat)>, SessionError> {
    let invalid = SessionError::InvalidUsageRecoveryIndex;
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let file = match fs::openat(recovery.as_fd(), id, flags, Mode::empty()) {
        Ok(file) => std::fs::File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(_) => return Err(invalid),
    };
    let stat = fs::fstat(&file).map_err(|_| invalid)?;
    let size = usize::try_from(stat.st_size).unwrap_or(usize::MAX);
    if file_type(&stat) != FileType::RegularFile
        || stat.st_nlink != 1
        || size == 0
        || size > MAX_MARKER_BYTES
        || permissions(&stat) != private_file_mode()
    {
        return Err(invalid);
    }
    let mut bytes = vec![0; size];
    file.read_exact_at(&mut bytes, 0).map_err(|_| invalid)?;
    let protected = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.strip_prefix(MARKER_PREFIX))
        .and_then(|text| text.strip_suffix('\n'))
        .filter(|digits| !digits.is_empty())
        .and_then(|digits| digits.parse::<i64>().ok())
        .filter(|protected| *protected >= 0)
        .ok_or(invalid)?;
    Ok(Some((protected, stat)))
}

pub(crate) fn checkpoint_modified_at_ns(dir: &PrivateDir, name: &str) -> Option<i128> {
    let stat = fs::statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    (file_type(&stat) == FileType::RegularFile && stat.st_nlink == 1).then(|| modified_at_ns(&stat))
}

#[cfg(test)]
mod tests;
