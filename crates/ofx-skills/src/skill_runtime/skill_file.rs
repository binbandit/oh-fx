use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::path::Path;

use ofx_workspace::{
    FileIdentity, FileKind, PathError, RegularFileError, entry_identity, open_directory,
    open_regular_file_at, open_regular_file_following_at, opened_file_path,
};
use rustix::fs::CWD;

use super::SymlinkAuthorities;
use crate::io::FileFreshness;
use crate::skill_contract::{
    InvalidMetadataCause, MetadataPrefixError, SKILL_FILE_NAME, SkillMetadata, parse_skill_file,
    read_metadata_prefix, resolve_metadata,
};

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
    authorities: &SymlinkAuthorities,
) -> Result<OwnedFd, DirectoryOpenError> {
    let canonical = fs::canonicalize(logical_path)
        .map_err(|error| DirectoryOpenError::Path(PathError::from(error)))?;
    if !authorities.allows(read_authority, &canonical) {
        return Err(DirectoryOpenError::OutsideReadAuthority);
    }
    open_directory(&canonical).map_err(DirectoryOpenError::Path)
}

pub(crate) enum PrimarySkillFile {
    Opened(File),
    Missing,
    Rejected,
}

pub(crate) struct SkillCandidate<'a> {
    pub(crate) directory: &'a OwnedFd,
    pub(crate) path: &'a Path,
    pub(crate) read_authority: Option<&'a Path>,
}

pub(crate) fn open_primary_skill_file(
    candidate: &SkillCandidate<'_>,
    authorities: &SymlinkAuthorities,
) -> PrimarySkillFile {
    let name = OsStr::new(SKILL_FILE_NAME);
    match entry_identity(candidate.directory, name).map(FileIdentity::kind) {
        Ok(FileKind::RegularFile) => match open_regular_file_at(candidate.directory, name) {
            Ok((file, _)) => PrimarySkillFile::Opened(file),
            Err(RegularFileError::Path(PathError::FileNotFound)) => PrimarySkillFile::Missing,
            Err(_) => PrimarySkillFile::Rejected,
        },
        Ok(FileKind::Symlink) => candidate
            .read_authority
            .and_then(|authority| {
                LinkedSkillFile::preflight(candidate.path, authority, authorities)
            })
            .and_then(|linked| linked.open(candidate.directory))
            .map_or(PrimarySkillFile::Rejected, PrimarySkillFile::Opened),
        Err(PathError::FileNotFound) => PrimarySkillFile::Missing,
        Ok(_) | Err(_) => PrimarySkillFile::Rejected,
    }
}

pub(super) struct LinkedSkillFile<'a> {
    read_authority: &'a Path,
    authorities: &'a SymlinkAuthorities,
}

impl<'a> LinkedSkillFile<'a> {
    pub(super) fn preflight(
        candidate_path: &Path,
        read_authority: &'a Path,
        authorities: &'a SymlinkAuthorities,
    ) -> Option<Self> {
        let target = fs::canonicalize(candidate_path.join(SKILL_FILE_NAME)).ok()?;
        if !authorities.allows(read_authority, &target) {
            return None;
        }
        let kind = entry_identity(CWD, target.as_os_str()).ok()?.kind();
        (kind == FileKind::RegularFile).then_some(Self {
            read_authority,
            authorities,
        })
    }

    pub(super) fn open(&self, candidate: &OwnedFd) -> Option<File> {
        let name = OsStr::new(SKILL_FILE_NAME);
        let (file, _) = open_regular_file_following_at(candidate, name).ok()?;
        let opened = opened_file_path(&file)?;
        self.authorities
            .allows(self.read_authority, &opened)
            .then_some(file)
    }
}

pub(crate) enum Inspection {
    Valid(SkillMetadata, FileFreshness),
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
    let freshness = FileFreshness::of(&metadata);
    let Ok(file_size) = usize::try_from(metadata.len()) else {
        return Inspection::Oversized;
    };
    let content = match read_metadata_prefix(file, file_size) {
        Ok(content) => content,
        Err(MetadataPrefixError::StreamTooLong) => return Inspection::Oversized,
        Err(MetadataPrefixError::Unreadable | MetadataPrefixError::Operational(_)) => {
            return Inspection::Unreadable;
        }
    };
    match resolve_metadata(&parse_skill_file(&content), fallback_name) {
        Ok(metadata) => Inspection::Valid(metadata, freshness),
        Err(cause) => Inspection::Invalid(cause),
    }
}
