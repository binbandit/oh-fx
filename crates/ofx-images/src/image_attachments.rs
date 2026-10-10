use std::fs::File;
use std::io::{self, Read};

use ofx_contract::ImageAttachment;
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::image_data::detect_media_type;

mod snapshots;

pub use snapshots::{
    CaptureBudget, VerifiedSnapshot, capture_image_snapshots, cleanup_snapshot_dir,
    create_temp_snapshot_dir, load_verified_snapshot,
};

const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const HEADER_BYTES: u64 = 64;
const PATH_WHITESPACE: &[char] = &[' ', '\t', '\r', '\n'];

pub const IMAGE_TOO_LARGE_NOTICE: &str = "image exceeds the 20 MiB limit";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AttachmentError {
    #[error("FileNotFound")]
    FileNotFound,
    #[error("AccessDenied")]
    AccessDenied,
    #[error("PermissionDenied")]
    PermissionDenied,
    #[error("SymLinkLoop")]
    SymLinkLoop,
    #[error("NotDir")]
    NotDir,
    #[error("IsDir")]
    IsDir,
    #[error("NameTooLong")]
    NameTooLong,
    #[error("NoDevice")]
    NoDevice,
    #[error("ProcessFdQuotaExceeded")]
    ProcessFdQuotaExceeded,
    #[error("SystemFdQuotaExceeded")]
    SystemFdQuotaExceeded,
    #[error("InputOutput")]
    InputOutput,
    #[error("NotRegularFile")]
    NotRegularFile,
    #[error("ImageTooLarge")]
    ImageTooLarge,
    #[error("UnsupportedImageType")]
    UnsupportedImageType,
    #[error("InvalidImageId")]
    InvalidImageId,
    #[error("ImageSnapshotPathUnsafe")]
    ImageSnapshotPathUnsafe,
    #[error("MissingImageSnapshot")]
    MissingImageSnapshot,
    #[error("InvalidImageSnapshotDigest")]
    InvalidImageSnapshotDigest,
    #[error("ImageSnapshotCorrupt")]
    ImageSnapshotCorrupt,
    #[error("ImageSnapshotMediaTypeMismatch")]
    ImageSnapshotMediaTypeMismatch,
    #[error("PathAlreadyExists")]
    PathAlreadyExists,
    #[error("NoSpaceLeft")]
    NoSpaceLeft,
    #[error("ReadOnlyFileSystem")]
    ReadOnlyFileSystem,
    #[error("DiskQuota")]
    DiskQuota,
    #[error("Cancelled")]
    Cancelled,
    #[error("Unexpected")]
    Unexpected,
}

impl From<Errno> for AttachmentError {
    fn from(errno: Errno) -> Self {
        match errno {
            Errno::NOENT => Self::FileNotFound,
            Errno::ACCESS => Self::AccessDenied,
            Errno::PERM => Self::PermissionDenied,
            Errno::LOOP => Self::SymLinkLoop,
            Errno::NOTDIR => Self::NotDir,
            Errno::ISDIR => Self::IsDir,
            Errno::NAMETOOLONG => Self::NameTooLong,
            Errno::NXIO | Errno::NODEV => Self::NoDevice,
            Errno::MFILE => Self::ProcessFdQuotaExceeded,
            Errno::NFILE => Self::SystemFdQuotaExceeded,
            Errno::IO => Self::InputOutput,
            Errno::EXIST => Self::PathAlreadyExists,
            Errno::NOSPC => Self::NoSpaceLeft,
            Errno::ROFS => Self::ReadOnlyFileSystem,
            Errno::DQUOT => Self::DiskQuota,
            _ => Self::Unexpected,
        }
    }
}

impl From<io::Error> for AttachmentError {
    fn from(error: io::Error) -> Self {
        Errno::from_io_error(&error).map_or(Self::Unexpected, Self::from)
    }
}

pub fn normalize_path_input(input: &str) -> String {
    let unquoted = strip_balanced_outer_quotes(input.trim_matches(PATH_WHITESPACE));
    let mut normalized = String::with_capacity(unquoted.len());
    let mut characters = unquoted.chars();
    while let Some(character) = characters.next() {
        if character == '\\'
            && let Some(escaped) = characters.next()
        {
            normalized.push(escaped);
            continue;
        }
        normalized.push(character);
    }
    normalized
}

fn strip_balanced_outer_quotes(text: &str) -> &str {
    ['"', '\'']
        .into_iter()
        .find_map(|quote| {
            text.strip_prefix(quote)
                .and_then(|inner| inner.strip_suffix(quote))
        })
        .unwrap_or(text)
}

pub fn load_resolved_image_attachment(path: String) -> Result<ImageAttachment, AttachmentError> {
    let (mut file, size) = open_image_source(&path)?;
    let header = read_image_header(&mut file, size)?;
    let media_type = detect_media_type(&header).ok_or(AttachmentError::UnsupportedImageType)?;
    Ok(ImageAttachment {
        path,
        media_type: media_type.to_owned(),
        ..ImageAttachment::default()
    })
}

fn open_image_source(path: &str) -> Result<(File, u64), AttachmentError> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let file = File::from(rustix::fs::open(path, flags, Mode::empty())?);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(AttachmentError::NotRegularFile);
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(AttachmentError::ImageTooLarge);
    }
    Ok((file, metadata.len()))
}

fn read_image_header(file: &mut impl Read, expected_size: u64) -> io::Result<Vec<u8>> {
    let mut header = Vec::new();
    file.take(expected_size.min(HEADER_BYTES))
        .read_to_end(&mut header)?;
    Ok(header)
}

#[cfg(test)]
mod tests;
