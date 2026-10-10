use std::ffi::CString;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_contract::ImageAttachment;
use ofx_text::lowercase_hex;
use rustix::fs::{AtFlags, Dir, Mode, OFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use super::{AttachmentError, MAX_IMAGE_BYTES, open_image_source};
use crate::image_data::detect_media_type;

const TRANSFER_BYTES: usize = 64 * 1024;
const SNAPSHOT_HEADER_BYTES: usize = 64;
const DIGEST_HEX_BYTES: usize = 64;
const SNAPSHOT_NAME_DIGEST_BYTES: usize = 16;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const TEMP_ROOT: &str = "/tmp";
const TEMP_DIRECTORY_PREFIX: &str = "oh-fx-image-snapshots-";
const TEMP_DIRECTORY_ATTEMPTS: usize = 16;

pub type CaptureBudget<'a> = &'a dyn Fn() -> Result<(), AttachmentError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSnapshot {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
}

struct StreamedSnapshot {
    digest_hex: String,
    media_type: &'static str,
}

#[derive(Debug)]
pub struct TempSnapshotDir {
    path: String,
}

impl TempSnapshotDir {
    pub fn create() -> Result<Self, AttachmentError> {
        let root = fs::canonicalize(TEMP_ROOT)?
            .into_os_string()
            .into_string()
            .map_err(|_| AttachmentError::ImageSnapshotPathUnsafe)?;
        for _ in 0..TEMP_DIRECTORY_ATTEMPTS {
            let mut suffix = [0_u8; 8];
            getrandom::fill(&mut suffix).map_err(|_| AttachmentError::Unexpected)?;
            let path = format!(
                "{root}/{TEMP_DIRECTORY_PREFIX}{:x}",
                u64::from_ne_bytes(suffix)
            );
            match DirBuilder::new().mode(PRIVATE_DIRECTORY_MODE).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(AttachmentError::PathAlreadyExists)
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for TempSnapshotDir {
    fn drop(&mut self) {
        cleanup_snapshot_dir(&self.path);
    }
}

fn cleanup_snapshot_dir(path: &str) {
    let Some((parent, name)) = split_parent(path) else {
        return;
    };
    if validate_component(name).is_err() {
        return;
    }
    if let Ok(parent) = open_directory_no_follow(parent) {
        let _ = delete_tree(&parent, name);
    }
}

fn delete_tree<Name: rustix::path::Arg + Copy>(parent: &OwnedFd, name: Name) -> Result<(), Errno> {
    match rustix::fs::unlinkat(parent, name, AtFlags::empty()) {
        Err(Errno::ISDIR | Errno::PERM) => {}
        unlinked => return unlinked,
    }
    let directory = rustix::fs::openat(parent, name, directory_flags(), Mode::empty())?;
    for entry in entry_names(&directory)? {
        delete_tree(&directory, entry.as_c_str())?;
    }
    rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)
}

fn entry_names(directory: &OwnedFd) -> Result<Vec<CString>, Errno> {
    let mut names = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name != c"." && name != c".." {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

pub fn capture_image_snapshots(
    attachments: &mut [ImageAttachment],
    snapshot_dir: &str,
    budget: CaptureBudget<'_>,
) -> Result<(), AttachmentError> {
    for index in 0..attachments.len() {
        if let Err(error) = capture_image_snapshot(&mut attachments[index], snapshot_dir, budget) {
            discard_image_snapshots(&mut attachments[..index]);
            return Err(error);
        }
    }
    Ok(())
}

fn capture_image_snapshot(
    attachment: &mut ImageAttachment,
    snapshot_dir: &str,
    budget: CaptureBudget<'_>,
) -> Result<(), AttachmentError> {
    if attachment.id == 0 {
        return Err(AttachmentError::InvalidImageId);
    }
    budget()?;
    let (mut source, _) = open_image_source(&attachment.path)?;
    let directory = open_or_create_snapshot_directory(snapshot_dir)?;
    let temp_name = format!("image-{}.source.{}", attachment.id, timestamp_nanos());
    let streamed = stream_source_to_file(&mut source, &directory, &temp_name, budget)
        .and_then(|streamed| budget().map(|()| streamed))
        .and_then(|streamed| {
            let final_name = format!(
                "image-{}-{}.bin",
                attachment.id,
                &streamed.digest_hex[..SNAPSHOT_NAME_DIGEST_BYTES]
            );
            budget()?;
            rustix::fs::renameat(
                &directory,
                temp_name.as_str(),
                &directory,
                final_name.as_str(),
            )?;
            Ok((streamed, final_name))
        });
    let (streamed, final_name) = match streamed {
        Ok(renamed) => renamed,
        Err(error) => {
            delete_snapshot_file(&directory, &temp_name);
            return Err(error);
        }
    };
    if let Err(error) = sync_directory(&directory).and_then(|()| budget()) {
        delete_snapshot_file(&directory, &final_name);
        return Err(error);
    }
    let final_path = format!("{}/{final_name}", snapshot_dir.trim_end_matches('/'));
    let previous = attachment.snapshot_path.replace(final_path.clone());
    attachment.snapshot_sha256 = Some(streamed.digest_hex);
    streamed.media_type.clone_into(&mut attachment.media_type);
    if let Some(previous) = previous.filter(|previous| *previous != final_path) {
        delete_snapshot_path(&previous);
    }
    Ok(())
}

fn stream_source_to_file(
    source: &mut File,
    directory: &OwnedFd,
    name: &str,
    budget: CaptureBudget<'_>,
) -> Result<StreamedSnapshot, AttachmentError> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut destination = File::from(rustix::fs::openat(
        directory,
        name,
        flags,
        Mode::RUSR | Mode::WUSR,
    )?);
    let mut hasher = Sha256::new();
    let mut header = Vec::with_capacity(SNAPSHOT_HEADER_BYTES);
    let mut buffer = vec![0; TRANSFER_BYTES];
    let mut written: u64 = 0;
    loop {
        budget()?;
        let count = read_some(source, &mut buffer)?;
        if count == 0 {
            break;
        }
        if count as u64 > MAX_IMAGE_BYTES - written {
            return Err(AttachmentError::ImageTooLarge);
        }
        let header_bytes = count.min(SNAPSHOT_HEADER_BYTES - header.len());
        header.extend_from_slice(&buffer[..header_bytes]);
        hasher.update(&buffer[..count]);
        destination.write_all(&buffer[..count])?;
        written += count as u64;
    }
    budget()?;
    destination.sync_all()?;
    let media_type = detect_media_type(&header).ok_or(AttachmentError::UnsupportedImageType)?;
    Ok(StreamedSnapshot {
        digest_hex: lowercase_hex(&hasher.finalize()),
        media_type,
    })
}

pub fn load_verified_snapshot(
    attachment: &ImageAttachment,
    budget: CaptureBudget<'_>,
) -> Result<VerifiedSnapshot, AttachmentError> {
    if let Some(inline) = &attachment.inline_data {
        let expected = expected_digest(attachment)?;
        budget()?;
        if inline.len() as u64 > MAX_IMAGE_BYTES {
            return Err(AttachmentError::ImageTooLarge);
        }
        return verified(inline.clone(), expected, attachment);
    }
    let path = attachment
        .snapshot_path
        .as_deref()
        .ok_or(AttachmentError::MissingImageSnapshot)?;
    let expected = expected_digest(attachment)?;
    budget()?;
    let mut file = open_snapshot_file_no_follow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(AttachmentError::NotRegularFile);
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(AttachmentError::ImageTooLarge);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    let mut buffer = vec![0; TRANSFER_BYTES];
    loop {
        budget()?;
        let count = read_some(&mut file, &mut buffer)?;
        if count == 0 {
            break;
        }
        if (bytes.len() + count) as u64 > MAX_IMAGE_BYTES {
            return Err(AttachmentError::ImageTooLarge);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    verified(bytes, expected, attachment)
}

fn expected_digest(attachment: &ImageAttachment) -> Result<&str, AttachmentError> {
    let expected = attachment
        .snapshot_sha256
        .as_deref()
        .ok_or(AttachmentError::MissingImageSnapshot)?;
    if expected.len() == DIGEST_HEX_BYTES {
        Ok(expected)
    } else {
        Err(AttachmentError::InvalidImageSnapshotDigest)
    }
}

fn verified(
    bytes: Vec<u8>,
    expected: &str,
    attachment: &ImageAttachment,
) -> Result<VerifiedSnapshot, AttachmentError> {
    if lowercase_hex(&Sha256::digest(&bytes)) != expected {
        return Err(AttachmentError::ImageSnapshotCorrupt);
    }
    let media_type = detect_media_type(&bytes).ok_or(AttachmentError::UnsupportedImageType)?;
    if media_type != attachment.media_type {
        return Err(AttachmentError::ImageSnapshotMediaTypeMismatch);
    }
    Ok(VerifiedSnapshot { bytes, media_type })
}

fn discard_image_snapshots(attachments: &mut [ImageAttachment]) {
    for index in 0..attachments.len() {
        if let Some(path) = &attachments[index].snapshot_path {
            let duplicate = attachments[..index]
                .iter()
                .any(|previous| previous.snapshot_path.as_ref() == Some(path));
            if !duplicate {
                delete_snapshot_path(path);
            }
        }
        let attachment = &mut attachments[index];
        attachment.snapshot_path = None;
        attachment.snapshot_sha256 = None;
    }
}

fn read_some(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match file.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            read => return read,
        }
    }
}

fn timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

fn sync_directory(directory: &OwnedFd) -> Result<(), AttachmentError> {
    match rustix::fs::fsync(directory) {
        Ok(()) | Err(Errno::INVAL | Errno::NOTSUP) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn delete_snapshot_file(directory: &OwnedFd, name: &str) {
    let _ = rustix::fs::unlinkat(directory, name, AtFlags::empty());
}

fn delete_snapshot_path(path: &str) {
    if let Some((parent, name)) = split_parent(path)
        && let Ok(directory) = open_directory_no_follow(parent)
    {
        delete_snapshot_file(&directory, name);
    }
}

fn unsafe_path_error(errno: Errno) -> AttachmentError {
    match errno {
        Errno::LOOP | Errno::NOTDIR | Errno::ISDIR => AttachmentError::ImageSnapshotPathUnsafe,
        other => other.into(),
    }
}

fn validate_component(component: &str) -> Result<(), AttachmentError> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains(['/', '\\'])
    {
        Err(AttachmentError::ImageSnapshotPathUnsafe)
    } else {
        Ok(())
    }
}

fn split_parent(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.trim_end_matches('/');
    let slash = trimmed.rfind('/')?;
    let parent = trimmed[..slash].trim_end_matches('/');
    Some((
        if parent.is_empty() { "/" } else { parent },
        &trimmed[slash + 1..],
    ))
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn open_directory_no_follow(path: &str) -> Result<OwnedFd, AttachmentError> {
    if !path.starts_with('/') {
        return Err(AttachmentError::ImageSnapshotPathUnsafe);
    }
    let mut directory =
        rustix::fs::open("/", directory_flags(), Mode::empty()).map_err(unsafe_path_error)?;
    for component in path.split('/').filter(|component| !component.is_empty()) {
        validate_component(component)?;
        directory = rustix::fs::openat(&directory, component, directory_flags(), Mode::empty())
            .map_err(unsafe_path_error)?;
    }
    Ok(directory)
}

fn snapshot_parent(path: &str) -> Result<(OwnedFd, &str), AttachmentError> {
    if !path.starts_with('/') {
        return Err(AttachmentError::ImageSnapshotPathUnsafe);
    }
    let (parent, name) = split_parent(path).ok_or(AttachmentError::ImageSnapshotPathUnsafe)?;
    validate_component(name)?;
    Ok((open_directory_no_follow(parent)?, name))
}

fn open_or_create_snapshot_directory(path: &str) -> Result<OwnedFd, AttachmentError> {
    let (parent, name) = snapshot_parent(path)?;
    match rustix::fs::openat(&parent, name, directory_flags(), Mode::empty()) {
        Ok(directory) => Ok(directory),
        Err(Errno::NOENT) => {
            match rustix::fs::mkdirat(&parent, name, Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(unsafe_path_error(error)),
            }
            rustix::fs::openat(&parent, name, directory_flags(), Mode::empty())
                .map_err(unsafe_path_error)
        }
        Err(error) => Err(unsafe_path_error(error)),
    }
}

fn open_snapshot_file_no_follow(path: &str) -> Result<File, AttachmentError> {
    let (parent, name) = snapshot_parent(path)?;
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let file = File::from(
        rustix::fs::openat(&parent, name, flags, Mode::empty()).map_err(unsafe_path_error)?,
    );
    if file.metadata()?.is_dir() {
        return Err(AttachmentError::ImageSnapshotPathUnsafe);
    }
    Ok(file)
}

#[cfg(test)]
mod tests;
