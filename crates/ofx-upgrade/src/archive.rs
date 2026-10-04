use std::fs::{self, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, SystemTime};

use flate2::read::GzDecoder;
use ofx_text::lowercase_hex;
use sha2::{Digest, Sha256};

use crate::error::UpgradeError;

const BINARY_NAME: &str = "oh-fx";
const EXECUTABLE_MODE: u32 = 0o755;
const BUSY_RETRY_LIMIT: u32 = 50;
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(20);
const STAGING_PREFIX: &str = ".oh-fx-upgrade-";
const STALE_STAGING_AGE: Duration = Duration::from_hours(1);

pub(crate) fn verify_checksum(archive: &[u8], checksum_file: &str) -> Result<(), UpgradeError> {
    let actual = lowercase_hex(&Sha256::digest(archive));
    let matches = checksum_file
        .split_whitespace()
        .next()
        .is_some_and(|expected| expected.eq_ignore_ascii_case(&actual));
    if matches {
        Ok(())
    } else {
        Err(UpgradeError::ChecksumMismatch)
    }
}

pub(crate) fn extract_binary(archive: &[u8]) -> Result<Vec<u8>, UpgradeError> {
    let mut entries = tar::Archive::new(GzDecoder::new(archive));
    let entries = entries
        .entries()
        .map_err(|_| UpgradeError::ExtractionFailed)?;
    for entry in entries {
        let mut entry = entry.map_err(|_| UpgradeError::ExtractionFailed)?;
        let is_binary = entry.header().entry_type().is_file()
            && entry.size() > 0
            && entry.path().is_ok_and(|path| is_root_binary_entry(&path));
        if is_binary {
            let mut binary = Vec::new();
            entry
                .read_to_end(&mut binary)
                .map_err(|_| UpgradeError::ExtractionFailed)?;
            return Ok(binary);
        }
    }
    Err(UpgradeError::ExtractionFailed)
}

pub(crate) fn ensure_replaceable(target: &Path) -> Result<(), UpgradeError> {
    let metadata = fs::metadata(target).map_err(|_| UpgradeError::SelfExeNotFound)?;
    let directory = target.parent().ok_or(UpgradeError::SelfExeNotFound)?;
    if metadata.permissions().readonly() || tempfile::tempfile_in(directory).is_err() {
        return Err(UpgradeError::ReplaceFailed);
    }
    Ok(())
}

pub(crate) fn install_executable(
    target: &Path,
    binary: &[u8],
    expected_version: &str,
) -> Result<(), UpgradeError> {
    ensure_replaceable(target)?;
    let directory = target.parent().ok_or(UpgradeError::SelfExeNotFound)?;
    sweep_stale_staging(directory, SystemTime::now());
    let staged = stage(directory, binary).map_err(|_| UpgradeError::ReplaceFailed)?;
    if reported_version(&staged).as_deref() != Some(expected_version) {
        return Err(UpgradeError::BinaryVerificationFailed);
    }
    staged
        .persist(target)
        .map_err(|_| UpgradeError::ReplaceFailed)
}

fn sweep_stale_staging(directory: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(STAGING_PREFIX.as_bytes())
            && entry.file_type().is_ok_and(|kind| kind.is_file())
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| {
                    now.duration_since(modified)
                        .is_ok_and(|age| age >= STALE_STAGING_AGE)
                });
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn stage(directory: &Path, binary: &[u8]) -> io::Result<tempfile::TempPath> {
    let mut staged = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempfile_in(directory)?;
    staged.write_all(binary)?;
    staged
        .as_file()
        .set_permissions(Permissions::from_mode(EXECUTABLE_MODE))?;
    staged.as_file().sync_all()?;
    Ok(staged.into_temp_path())
}

fn reported_version(executable: &Path) -> Option<String> {
    let output = run_version_command(executable).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn run_version_command(executable: &Path) -> io::Result<Output> {
    let mut busy_retries = 0;
    loop {
        match Command::new(executable).arg("--version").output() {
            Err(error)
                if error.kind() == io::ErrorKind::ExecutableFileBusy
                    && busy_retries < BUSY_RETRY_LIMIT =>
            {
                busy_retries += 1;
                thread::sleep(BUSY_RETRY_DELAY);
            }
            result => return result,
        }
    }
}

fn is_root_binary_entry(path: &Path) -> bool {
    let mut components = path
        .components()
        .filter(|component| *component != Component::CurDir);
    components.next() == Some(Component::Normal(BINARY_NAME.as_ref()))
        && components.next().is_none()
}

#[cfg(test)]
pub(crate) fn version_script(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho {version}\n").into_bytes()
}

#[cfg(test)]
pub(crate) fn sha256_line(archive: &[u8], name: &str) -> String {
    format!("{}  {name}\n", lowercase_hex(&Sha256::digest(archive)))
}

#[cfg(test)]
pub(crate) fn archive_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    for (path, contents) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, *contents).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;

    use super::*;

    #[test]
    fn installing_sweeps_only_stale_staged_copies() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let aged = |path: &Path| {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_hours(2))
                .unwrap();
        };
        for name in [".oh-fx-upgrade-stale", ".oh-fx-upgrade-fresh", "notes.txt"] {
            fs::write(root.join(name), b"x").unwrap();
        }
        aged(&root.join(".oh-fx-upgrade-stale"));
        aged(&root.join("notes.txt"));
        fs::create_dir(root.join(".oh-fx-upgrade-folder")).unwrap();
        let target = root.join("oh-fx");
        fs::write(&target, version_script("0.1.0")).unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o755)).unwrap();
        install_executable(&target, &version_script("0.2.0"), "0.2.0").unwrap();
        assert!(!root.join(".oh-fx-upgrade-stale").exists());
        assert!(root.join(".oh-fx-upgrade-fresh").exists());
        assert!(root.join("notes.txt").exists());
        assert!(root.join(".oh-fx-upgrade-folder").is_dir());
    }

    #[test]
    fn extracts_only_the_root_binary() {
        let archive = archive_with(&[("LICENSE", b"license"), ("oh-fx", b"binary")]);
        assert_eq!(extract_binary(&archive).unwrap(), b"binary");
    }

    #[test]
    fn rejects_archives_without_a_root_binary() {
        let archive = archive_with(&[("nested/oh-fx", b"binary")]);
        assert!(matches!(
            extract_binary(&archive),
            Err(UpgradeError::ExtractionFailed)
        ));
    }

    #[test]
    fn rejects_empty_or_non_file_binary_entries() {
        let empty = archive_with(&[("oh-fx", b"")]);
        assert!(matches!(
            extract_binary(&empty),
            Err(UpgradeError::ExtractionFailed)
        ));
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "oh-fx", "/bin/sh")
            .unwrap();
        let symlink = builder.into_inner().unwrap().finish().unwrap();
        assert!(matches!(
            extract_binary(&symlink),
            Err(UpgradeError::ExtractionFailed)
        ));
    }

    #[test]
    fn verifies_sha256sum_files_case_insensitively() {
        let checksum_file = sha256_line(b"release", "oh-fx-linux-x86_64.tar.gz");
        assert!(verify_checksum(b"release", &checksum_file).is_ok());
        assert!(verify_checksum(b"release", &checksum_file.to_ascii_uppercase()).is_ok());
        assert!(matches!(
            verify_checksum(b"tampered", &checksum_file),
            Err(UpgradeError::ChecksumMismatch)
        ));
        assert!(matches!(
            verify_checksum(b"release", ""),
            Err(UpgradeError::ChecksumMismatch)
        ));
    }

    fn installed_executable(directory: &tempfile::TempDir, mode: u32) -> std::path::PathBuf {
        let target = directory.path().join("oh-fx");
        fs::write(&target, version_script("0.1.0-dev.1")).unwrap();
        fs::set_permissions(&target, Permissions::from_mode(mode)).unwrap();
        target
    }

    #[test]
    fn installs_a_binary_that_reports_the_expected_version() {
        let directory = tempfile::tempdir().unwrap();
        let target = installed_executable(&directory, 0o755);
        let binary = version_script("0.1.0-dev.2");
        install_executable(&target, &binary, "0.1.0-dev.2").unwrap();
        assert_eq!(fs::read(&target).unwrap(), binary);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn keeps_the_old_binary_when_the_new_one_reports_another_version() {
        let directory = tempfile::tempdir().unwrap();
        let target = installed_executable(&directory, 0o755);
        let result = install_executable(&target, &version_script("0.1.0-dev.3"), "0.1.0-dev.2");
        assert!(matches!(
            result,
            Err(UpgradeError::BinaryVerificationFailed)
        ));
        assert_eq!(fs::read(&target).unwrap(), version_script("0.1.0-dev.1"));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn waits_for_a_staged_binary_to_close_for_writing() {
        let directory = tempfile::tempdir().unwrap();
        let staged = directory.path().join("oh-fx");
        let mut writer = fs::File::create(&staged).unwrap();
        writer.write_all(&version_script("0.1.0-dev.2")).unwrap();
        fs::set_permissions(&staged, Permissions::from_mode(EXECUTABLE_MODE)).unwrap();
        assert_eq!(
            Command::new(&staged).output().unwrap_err().kind(),
            io::ErrorKind::ExecutableFileBusy
        );
        let release = thread::spawn(move || {
            thread::sleep(BUSY_RETRY_DELAY * 5);
            drop(writer);
        });
        assert_eq!(reported_version(&staged).as_deref(), Some("0.1.0-dev.2"));
        release.join().unwrap();
    }

    const INTERRUPTED_CHILD: &str = "OH_FX_UPGRADE_INTERRUPTED_INSTALL";
    const INTERRUPTED_TEST: &str =
        "archive::tests::an_install_killed_before_its_rename_leaves_the_old_binary_working";

    #[test]
    fn an_install_killed_before_its_rename_leaves_the_old_binary_working() {
        if let Some(root) = std::env::var_os(INTERRUPTED_CHILD) {
            let root = Path::new(&root);
            let binary = fs::read(root.join("release")).unwrap();
            let _ = install_executable(&root.join("oh-fx"), &binary, "0.1.0-dev.2");
            std::process::exit(0);
        }
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let target = root.join("oh-fx");
        fs::write(&target, version_script("0.1.0-dev.1")).unwrap();
        fs::set_permissions(&target, Permissions::from_mode(EXECUTABLE_MODE)).unwrap();
        let verifying = root.join("verifying");
        fs::write(
            root.join("release"),
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ -d '{}' ]; do sleep 0.01; done\necho 0.1.0-dev.2\n",
                verifying.display(),
                root.display()
            ),
        )
        .unwrap();
        let mut installer = Command::new(std::env::current_exe().unwrap())
            .args([INTERRUPTED_TEST, "--exact", "--test-threads=1"])
            .env(INTERRUPTED_CHILD, &root)
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let group = rustix::process::Pid::from_child(&installer);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !verifying.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the install never staged"
            );
            thread::sleep(Duration::from_millis(5));
        }
        rustix::process::kill_process_group(group, rustix::process::Signal::KILL).unwrap();
        installer.wait().unwrap();
        while rustix::process::test_kill_process_group(group).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the staged verifier outlived the installer"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(fs::read(&target).unwrap(), version_script("0.1.0-dev.1"));
        assert_eq!(reported_version(&target).as_deref(), Some("0.1.0-dev.1"));
        let entries: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        let leftovers = entries
            .iter()
            .filter(|name| {
                name.as_encoded_bytes()
                    .starts_with(STAGING_PREFIX.as_bytes())
            })
            .count();
        assert_eq!(leftovers, 1, "{entries:?}");
    }

    #[test]
    fn refuses_to_replace_a_read_only_binary() {
        let directory = tempfile::tempdir().unwrap();
        let target = installed_executable(&directory, 0o555);
        assert!(matches!(
            ensure_replaceable(&target),
            Err(UpgradeError::ReplaceFailed)
        ));
    }
}
