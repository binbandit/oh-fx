use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::path::Path;

use ofx_workspace::{
    FileIdentity, FileKind, PathError, RegularFileError, entry_identity, open_directory,
    open_regular_file_at, path_inside,
};

use crate::skill_contract::{
    InvalidMetadataCause, MetadataPrefixError, SkillMetadata, parse_skill_file,
    read_metadata_prefix, resolve_metadata,
};

pub(crate) const SKILL_FILE_NAME: &str = "SKILL.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryOpenError {
    Path(PathError),
    OutsideReadAuthority,
}

impl DirectoryOpenError {
    pub(crate) fn is_missing(self) -> bool {
        matches!(
            self,
            Self::Path(PathError::FileNotFound | PathError::NotDir)
        )
    }
}

pub(crate) fn open_contained_directory(
    logical_path: &Path,
    read_authority: &Path,
) -> Result<OwnedFd, DirectoryOpenError> {
    let canonical = fs::canonicalize(logical_path)
        .map_err(|error| DirectoryOpenError::Path(PathError::from(error)))?;
    if !path_inside(read_authority, &canonical) {
        return Err(DirectoryOpenError::OutsideReadAuthority);
    }
    open_directory(&canonical).map_err(DirectoryOpenError::Path)
}

pub(crate) enum PrimarySkillFile {
    Opened(File),
    Missing,
    Rejected,
}

pub(crate) fn open_primary_skill_file(candidate: &OwnedFd) -> PrimarySkillFile {
    let name = OsStr::new(SKILL_FILE_NAME);
    match entry_identity(candidate, name).map(FileIdentity::kind) {
        Ok(FileKind::RegularFile) => match open_regular_file_at(candidate, name) {
            Ok((file, _)) => PrimarySkillFile::Opened(file),
            Err(RegularFileError::Path(PathError::FileNotFound)) => PrimarySkillFile::Missing,
            Err(_) => PrimarySkillFile::Rejected,
        },
        Err(PathError::FileNotFound) => PrimarySkillFile::Missing,
        Ok(_) | Err(_) => PrimarySkillFile::Rejected,
    }
}

pub(crate) enum Inspection {
    Valid(SkillMetadata),
    Invalid(InvalidMetadataCause),
    Unreadable,
    Oversized,
}

pub(crate) fn inspect_skill_file(file: &File, fallback_name: &[u8]) -> Inspection {
    let Ok(metadata) = file.metadata() else {
        return Inspection::Unreadable;
    };
    if !metadata.is_file() {
        return Inspection::Unreadable;
    }
    let Ok(file_size) = usize::try_from(metadata.len()) else {
        return Inspection::Oversized;
    };
    let content = match read_metadata_prefix(file, file_size) {
        Ok(content) => content,
        Err(MetadataPrefixError::StreamTooLong) => return Inspection::Oversized,
        Err(MetadataPrefixError::Unreadable) => return Inspection::Unreadable,
    };
    match resolve_metadata(&parse_skill_file(&content), fallback_name) {
        Ok(metadata) => Inspection::Valid(metadata),
        Err(cause) => Inspection::Invalid(cause),
    }
}
