use std::fs::{self, File, TryLockError};
use std::io;
use std::path::Path;

const LOCK_FILE: &str = "upgrade.lock";

pub struct UpgradeLock {
    _file: File,
}

impl UpgradeLock {
    pub fn acquire(state_directory: &Path) -> io::Result<Self> {
        let file = open(state_directory)?;
        file.lock()?;
        Ok(Self { _file: file })
    }

    pub fn try_acquire(state_directory: &Path) -> Option<Self> {
        let file = open(state_directory).ok()?;
        match file.try_lock() {
            Ok(()) => Some(Self { _file: file }),
            Err(TryLockError::WouldBlock | TryLockError::Error(_)) => None,
        }
    }
}

fn open(state_directory: &Path) -> io::Result<File> {
    fs::create_dir_all(state_directory)?;
    File::create(state_directory.join(LOCK_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_a_second_upgrade_while_one_holds_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let _held = UpgradeLock::acquire(directory.path()).unwrap();
        assert!(UpgradeLock::try_acquire(directory.path()).is_none());
    }
}
