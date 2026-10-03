use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags};

use super::filesystem::{CopySource, DIRECTORY, copy_tree, make_directory, remove_tree};

pub(super) fn lock_directory(parent: &OwnedFd) -> io::Result<OwnedFd> {
    make_directory(
        parent,
        OsStr::new(".skill-install-locks"),
        Mode::from_raw_mode(0o700),
    )?;
    let directory = fs::openat(parent, ".skill-install-locks", DIRECTORY, Mode::empty())?;
    let stat = fs::fstat(&directory)?;
    if stat.st_uid != rustix::process::getuid().as_raw() || stat.st_mode & 0o077 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(directory)
}

fn lock(locks: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    lock_with(locks, name, |file| {
        fs::flock(file, FlockOperation::NonBlockingLockExclusive)
    })
}

fn lock_with(
    locks: &OwnedFd,
    name: &str,
    mut acquire: impl FnMut(&OwnedFd) -> rustix::io::Result<()>,
) -> io::Result<OwnedFd> {
    let file = fs::openat(
        locks,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let stat = fs::fstat(&file)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || stat.st_uid != rustix::process::getuid().as_raw()
        || stat.st_mode & 0o077 != 0
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match acquire(&file) {
            Ok(()) => {
                let named = fs::statat(locks, name, AtFlags::SYMLINK_NOFOLLOW)?;
                let held = fs::fstat(&file)?;
                if named.st_dev != held.st_dev || named.st_ino != held.st_ino {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                return Ok(file);
            }
            Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn transaction(root: &OwnedFd) -> io::Result<(String, OwnedFd)> {
    transaction_with_token(root, |bytes| {
        getrandom::fill(bytes).map_err(io::Error::other)
    })
}

fn transaction_with_token(
    root: &OwnedFd,
    mut fill: impl FnMut(&mut [u8; 16]) -> io::Result<()>,
) -> io::Result<(String, OwnedFd)> {
    for _ in 0..8 {
        let mut bytes = [0; 16];
        fill(&mut bytes)?;
        let suffix: String = bytes
            .iter()
            .flat_map(|byte| {
                let alphabet = b"0123456789abcdef";
                [
                    char::from(alphabet[usize::from(byte >> 4)]),
                    char::from(alphabet[usize::from(byte & 15)]),
                ]
            })
            .collect();
        let name = format!(".skill-install-{suffix}");
        match fs::mkdirat(root, &name, Mode::from_raw_mode(0o700)) {
            Ok(()) => {
                return Ok((
                    name.clone(),
                    fs::openat(root, name.as_str(), DIRECTORY, Mode::empty())?,
                ));
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}

pub(super) fn replace(
    input: CopySource<'_>,
    root: &OwnedFd,
    locks: &OwnedFd,
    name: &str,
) -> io::Result<()> {
    let (transaction_name, staging) = transaction(root)?;
    let mut preserve = false;
    let result = commit_with_rename(
        input,
        root,
        locks,
        name,
        &staging,
        &mut preserve,
        |from, old, to, new| {
            validate_transaction(root, &transaction_name, &staging)?;
            fs::renameat(from, old, to, new).map_err(Into::into)
        },
    );
    if !preserve && validate_transaction(root, &transaction_name, &staging).is_ok() {
        let _ = remove_tree(root, OsStr::new(&transaction_name));
    }
    result
}

fn validate_transaction(root: &OwnedFd, name: &str, held: &OwnedFd) -> io::Result<()> {
    let named = fs::statat(root, name, AtFlags::SYMLINK_NOFOLLOW)?;
    let held = fs::fstat(held)?;
    if named.st_dev != held.st_dev
        || named.st_ino != held.st_ino
        || FileType::from_raw_mode(named.st_mode) != FileType::Directory
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

fn commit_with_rename(
    input: CopySource<'_>,
    root: &OwnedFd,
    locks: &OwnedFd,
    name: &str,
    staging: &OwnedFd,
    preserve: &mut bool,
    mut rename: impl FnMut(&OwnedFd, &str, &OwnedFd, &str) -> io::Result<()>,
) -> io::Result<()> {
    fs::mkdirat(staging, "staged", Mode::from_raw_mode(0o777))?;
    let copied = fs::openat(staging, "staged", DIRECTORY, Mode::empty())?;
    copy_tree(input, &copied)?;
    let _lock = lock(locks, name)?;
    let moved = match fs::statat(root, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
                return Err(io::ErrorKind::InvalidData.into());
            }
            rename(root, name, staging, "backup")?;
            true
        }
        Err(rustix::io::Errno::NOENT) => false,
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = rename(staging, "staged", root, name) {
        if moved && rename(staging, "backup", root, name).is_err() {
            *preserve = true;
            return Err(io::Error::other("SkillInstallRollbackFailed"));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::filesystem::path_directory;
    use std::fs as stdfs;

    fn fixture() -> (tempfile::TempDir, OwnedFd, OwnedFd, OwnedFd) {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        stdfs::create_dir(base.join("source")).unwrap();
        stdfs::write(base.join("source/SKILL.md"), "new").unwrap();
        stdfs::create_dir(base.join("skills/review"))
            .or_else(|_| {
                stdfs::create_dir(base.join("skills"))?;
                stdfs::create_dir(base.join("skills/review"))
            })
            .unwrap();
        stdfs::write(base.join("skills/review/SKILL.md"), "old").unwrap();
        let input = path_directory(&base.join("source"), false).unwrap();
        let root = path_directory(&base.join("skills"), false).unwrap();
        let parent = path_directory(&base, false).unwrap();
        let locks = lock_directory(&parent).unwrap();
        (temp, input, root, locks)
    }

    #[test]
    fn transaction_collision_exhaustion_preserves_existing_skill() {
        let (temp, _, root, _) = fixture();
        let name = format!(".skill-install-{}", "00".repeat(16));
        fs::mkdirat(&root, name.as_str(), Mode::from_raw_mode(0o700)).unwrap();
        let mut calls = 0;
        let error = transaction_with_token(&root, |_| {
            calls += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(calls, 8);
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            stdfs::read(temp.path().join("skills/review/SKILL.md")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn failed_commit_restores_old_skill_and_holds_lock_through_rollback() {
        let (temp, input, root, locks) = fixture();
        let (name, staging) = transaction(&root).unwrap();
        let mut preserve = false;
        let mut calls = 0;
        let result = commit_with_rename(
            CopySource {
                directory: &input,
                skill: None,
            },
            &root,
            &locks,
            "review",
            &staging,
            &mut preserve,
            |from, old, to, new| {
                calls += 1;
                if calls == 2 {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                if calls == 3 {
                    let contender =
                        fs::openat(&locks, "review", OFlags::RDWR, Mode::empty()).unwrap();
                    assert_eq!(
                        fs::flock(&contender, FlockOperation::NonBlockingLockExclusive)
                            .unwrap_err(),
                        rustix::io::Errno::WOULDBLOCK
                    );
                }
                fs::renameat(from, old, to, new).map_err(Into::into)
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(!preserve);
        assert_eq!(calls, 3);
        assert_eq!(
            stdfs::read(temp.path().join("skills/review/SKILL.md")).unwrap(),
            b"old"
        );
        remove_tree(&root, OsStr::new(&name)).unwrap();
    }

    #[test]
    fn failed_rollback_keeps_backup_in_transaction() {
        let (temp, input, root, locks) = fixture();
        let (name, staging) = transaction(&root).unwrap();
        let mut preserve = false;
        let mut calls = 0;
        let error = commit_with_rename(
            CopySource {
                directory: &input,
                skill: None,
            },
            &root,
            &locks,
            "review",
            &staging,
            &mut preserve,
            |from, old, to, new| {
                calls += 1;
                if calls >= 2 {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                fs::renameat(from, old, to, new).map_err(Into::into)
            },
        )
        .unwrap_err();
        assert!(preserve);
        assert_eq!(error.to_string(), "SkillInstallRollbackFailed");
        assert_eq!(
            stdfs::read(
                temp.path()
                    .join("skills")
                    .join(name)
                    .join("backup/SKILL.md")
            )
            .unwrap(),
            b"old"
        );
    }
    #[test]
    fn busy_destination_lock_keeps_original_and_removes_staging() {
        let (temp, input, root, locks) = fixture();
        let held = lock(&locks, "review").unwrap();
        let error = replace(
            CopySource {
                directory: &input,
                skill: None,
            },
            &root,
            &locks,
            "review",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            stdfs::read(temp.path().join("skills/review/SKILL.md")).unwrap(),
            b"old"
        );
        assert!(
            super::super::filesystem::entries(&root)
                .unwrap()
                .iter()
                .all(|(name, _)| !name.as_encoded_bytes().starts_with(b".skill-install-"))
        );
        drop(held);
        replace(
            CopySource {
                directory: &input,
                skill: None,
            },
            &root,
            &locks,
            "review",
        )
        .unwrap();
        assert_eq!(
            stdfs::read(temp.path().join("skills/review/SKILL.md")).unwrap(),
            b"new"
        );
    }

    #[test]
    fn lock_leaves_preserve_long_unicode_names_and_destination_alias_identity() {
        let (_temp, _, _, locks) = fixture();
        for name in ["n".repeat(240), "Réview 🚀".to_owned()] {
            let held = lock(&locks, &name).unwrap();
            assert_eq!(fs::fstat(&held).unwrap().st_nlink, 1);
        }
        let held = lock(&locks, "review").unwrap();
        let alias = fs::openat(
            &locks,
            "Review",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .unwrap();
        let same = super::super::same_directory(&held, &alias).unwrap();
        let attempt = fs::flock(&alias, FlockOperation::NonBlockingLockExclusive);
        if same {
            assert_eq!(attempt.unwrap_err(), rustix::io::Errno::WOULDBLOCK);
        } else {
            attempt.unwrap();
        }
    }
    #[test]
    fn replaced_lock_inode_never_grants_install_authority() {
        let (_temp, _, _, locks) = fixture();
        let error = lock_with(&locks, "review", |file| {
            fs::renameat(&locks, "review", &locks, "moved").unwrap();
            fs::openat(
                &locks,
                "review",
                OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
                Mode::from_raw_mode(0o600),
            )
            .unwrap();
            fs::flock(file, FlockOperation::NonBlockingLockExclusive)
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
    #[test]
    fn replaced_skill_metadata_never_publishes_under_validated_name() {
        let (temp, input, root, locks) = fixture();
        let original = std::fs::File::open(temp.path().join("source/SKILL.md")).unwrap();
        let freshness = crate::io::FileFreshness::of(&original.metadata().unwrap());
        stdfs::rename(
            temp.path().join("source/SKILL.md"),
            temp.path().join("source/old.md"),
        )
        .unwrap();
        stdfs::write(temp.path().join("source/SKILL.md"), "replacement").unwrap();
        let error = replace(
            CopySource {
                directory: &input,
                skill: Some(freshness),
            },
            &root,
            &locks,
            "review",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            stdfs::read(temp.path().join("skills/review/SKILL.md")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn replaced_transaction_route_fails_before_destination_mutation() {
        let (_temp, _, root, _) = fixture();
        let (name, staging) = transaction(&root).unwrap();
        fs::renameat(&root, name.as_str(), &root, "moved").unwrap();
        fs::mkdirat(&root, name.as_str(), Mode::from_raw_mode(0o700)).unwrap();
        assert_eq!(
            validate_transaction(&root, &name, &staging)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn vanished_or_linked_validated_skill_never_replaces_existing_install() {
        for linked in [false, true] {
            let (temp, input, root, locks) = fixture();
            let path = temp.path().join("source/SKILL.md");
            let held = std::fs::File::open(&path).unwrap();
            let freshness = crate::io::FileFreshness::of(&held.metadata().unwrap());
            stdfs::rename(&path, temp.path().join("source/original.md")).unwrap();
            if linked {
                std::os::unix::fs::symlink("original.md", &path).unwrap();
            }
            let error = replace(
                CopySource {
                    directory: &input,
                    skill: Some(freshness),
                },
                &root,
                &locks,
                "review",
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(
                stdfs::read(temp.path().join("skills/review/SKILL.md")).unwrap(),
                b"old"
            );
        }
    }
}
