use std::fs::{self, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::error::UpgradeError;

const BINARY_NAME: &str = "oh-fx";
const EXECUTABLE_MODE: u32 = 0o755;
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const BUSY_RETRY_LIMIT: u32 = 50;
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(20);

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
    let staged = stage(directory, binary).map_err(|_| UpgradeError::ReplaceFailed)?;
    if reported_version(&staged).as_deref() != Some(expected_version) {
        return Err(UpgradeError::BinaryVerificationFailed);
    }
    staged
        .persist(target)
        .map_err(|_| UpgradeError::ReplaceFailed)
}

fn stage(directory: &Path, binary: &[u8]) -> io::Result<tempfile::TempPath> {
    let mut staged = tempfile::Builder::new()
        .prefix(".oh-fx-upgrade-")
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

fn lowercase_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| {
            [
                HEX_DIGITS[usize::from(byte >> 4)],
                HEX_DIGITS[usize::from(byte & 0x0f)],
            ]
        })
        .map(char::from)
        .collect()
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
    use super::*;

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
