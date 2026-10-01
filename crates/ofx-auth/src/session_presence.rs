use std::path::Path;

use crate::io::{DurableError, PrivateDir, nearest_existing_ancestor_writable};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presence {
    Present,
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("CredentialStorageUnavailable")]
pub(crate) struct StorageUnavailable;

pub(crate) fn profile_file(directory: &Path, file_name: &str, max_bytes: usize) -> Presence {
    let directory = match PrivateDir::open_existing(directory) {
        Ok(Some(directory)) => directory,
        Ok(None) => return Presence::Missing,
        Err(_) => return Presence::Unavailable,
    };
    match directory.private_file_present(file_name, max_bytes) {
        Some(true) => Presence::Present,
        Some(false) => Presence::Missing,
        None => Presence::Unavailable,
    }
}

pub(crate) fn require_writable_profile_file(
    directory: &Path,
    file_name: &str,
    lock_name: &str,
) -> Result<bool, StorageUnavailable> {
    let opened = match PrivateDir::open_existing(directory) {
        Ok(Some(opened)) => opened,
        Ok(None) => {
            return if nearest_existing_ancestor_writable(directory) {
                Ok(false)
            } else {
                Err(StorageUnavailable)
            };
        }
        Err(_) => return Err(StorageUnavailable),
    };
    let exists = require_writable_in_dir(&opened, file_name)?;
    require_writable_in_dir(&opened, lock_name)?;
    Ok(exists)
}

pub(crate) fn require_writable_in_dir(
    directory: &PrivateDir,
    file_name: &str,
) -> Result<bool, StorageUnavailable> {
    if !directory.owner_writable() {
        return Err(StorageUnavailable);
    }
    directory
        .private_file_exists(file_name)
        .map_err(|_: DurableError| StorageUnavailable)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn credential_write_admission_accepts_missing_files_and_rejects_read_only_targets() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("oh-fx");
        let directory = PrivateDir::open_or_create(&path).unwrap();
        assert_eq!(require_writable_in_dir(&directory, "auth.json"), Ok(false));
        directory.replace("auth.json", b"{}").unwrap();
        assert_eq!(require_writable_in_dir(&directory, "auth.json"), Ok(true));
        for mode in [0o400, 0o200, 0o700] {
            std::fs::set_permissions(
                path.join("auth.json"),
                std::fs::Permissions::from_mode(mode),
            )
            .unwrap();
            assert_eq!(
                require_writable_in_dir(&directory, "auth.json"),
                Err(StorageUnavailable)
            );
        }
    }

    #[test]
    fn missing_profile_directories_need_a_writable_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data/oh-fx");
        assert_eq!(profile_file(&path, "auth.json", 64), Presence::Missing);
        assert_eq!(
            require_writable_profile_file(&path, "auth.json", "auth.lock"),
            Ok(false)
        );
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let admitted = require_writable_profile_file(&path, "auth.json", "auth.lock");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(admitted, Err(StorageUnavailable));
    }

    #[test]
    fn profile_presence_requires_a_private_non_empty_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("oh-fx");
        let directory = PrivateDir::open_or_create(&path).unwrap();
        directory.replace("auth.json", b"").unwrap();
        assert_eq!(profile_file(&path, "auth.json", 64), Presence::Unavailable);
        directory.replace("auth.json", b"{}").unwrap();
        assert_eq!(profile_file(&path, "auth.json", 64), Presence::Present);
        assert_eq!(profile_file(&path, "auth.json", 1), Presence::Unavailable);
    }
}
