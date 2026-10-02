use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_workspace::{
    FileKind, PathError, RegularFileError, basename, dirname, entry_identity, open_child_directory,
    open_directory, open_regular_file_at,
};

use super::SymlinkAuthorities;
use super::skill_file::{
    DirectoryOpenError, Inspection, PrimarySkillFile, SkillCandidate, inspect_skill_file,
    open_contained_directory, open_primary_skill_file,
};
use crate::io::FileFreshness;
use crate::skill_contract::{SKILL_FILE_NAME, Skill, SkillDiagnosticCause};

const RESOURCE_PADDING: &[char] = &[' ', '\t', '\r', '\n'];

#[derive(Debug)]
pub(crate) struct OpenedSkillCandidate {
    directory: OwnedFd,
    skill_file: File,
    freshness: FileFreshness,
}

#[derive(Debug)]
pub(crate) enum CandidateOpen {
    Current(OpenedSkillCandidate),
    Missing,
    NameMismatch,
    Skipped(SkillDiagnosticCause),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceOpenError {
    InvalidPath,
    NotRegularFile,
    Path(PathError),
}

impl From<PathError> for ResourceOpenError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl OpenedSkillCandidate {
    pub(crate) fn skill_file(&self) -> &File {
        &self.skill_file
    }

    pub(crate) fn freshness(&self) -> FileFreshness {
        self.freshness
    }

    pub(crate) fn open_resource(
        &self,
        resource: &str,
    ) -> Result<(File, FileFreshness), ResourceOpenError> {
        let mut segments = resource_segments(resource).ok_or(ResourceOpenError::InvalidPath)?;
        let mut segment = valid_segment(segments.next())?;
        let Some(child) = segments.next() else {
            return open_resource_file(&self.directory, segment);
        };
        let mut next = valid_segment(Some(child))?;
        let mut current = open_child_directory(&self.directory, segment)?;
        for following in segments {
            let following = valid_segment(Some(following))?;
            segment = next;
            current = open_child_directory(&current, segment)?;
            next = following;
        }
        open_resource_file(&current, next)
    }
}

fn resource_segments(resource: &str) -> Option<impl Iterator<Item = &OsStr>> {
    let trimmed = resource.trim_matches(RESOURCE_PADDING);
    if trimmed.is_empty() || trimmed.starts_with('/') {
        return None;
    }
    Some(
        trimmed
            .split(['/', '\\'])
            .filter(|segment| !segment.is_empty())
            .map(OsStr::new),
    )
}

fn valid_segment(segment: Option<&OsStr>) -> Result<&OsStr, ResourceOpenError> {
    segment
        .filter(|segment| segment.as_bytes() != b"." && segment.as_bytes() != b"..")
        .ok_or(ResourceOpenError::InvalidPath)
}

fn open_resource_file(
    directory: &OwnedFd,
    name: &OsStr,
) -> Result<(File, FileFreshness), ResourceOpenError> {
    match entry_identity(directory, name)?.kind() {
        FileKind::RegularFile => open_regular_file_at(directory, name)
            .map(|(file, metadata)| (file, FileFreshness::of(&metadata)))
            .map_err(|error| match error {
                RegularFileError::NotRegularFile => ResourceOpenError::NotRegularFile,
                RegularFileError::Path(error) => ResourceOpenError::Path(error),
            }),
        FileKind::Directory => Err(ResourceOpenError::Path(PathError::IsDir)),
        FileKind::Symlink => Err(ResourceOpenError::Path(PathError::SymLinkLoop)),
        FileKind::Other => Err(ResourceOpenError::NotRegularFile),
    }
}

pub(crate) fn resource_is_skill_file(resource: &str) -> bool {
    resource_segments(resource).is_some_and(|mut segments| {
        segments.next() == Some(OsStr::new(SKILL_FILE_NAME)) && segments.next().is_none()
    })
}

pub(crate) fn open_validated_skill_candidate(
    skill: &Skill,
    authorities: &SymlinkAuthorities,
) -> CandidateOpen {
    let path = skill.path.as_os_str().as_bytes();
    let candidate_name = basename(path);
    if candidate_name.is_empty() {
        return CandidateOpen::Skipped(SkillDiagnosticCause::Unreadable);
    }
    let opened = match &skill.read_authority {
        Some(authority) => open_contained_directory(&skill.path, authority, authorities),
        None => match dirname(path) {
            Some(parent) => open_directory(Path::new(OsStr::from_bytes(parent)))
                .and_then(|parent| open_child_directory(&parent, OsStr::from_bytes(candidate_name)))
                .map_err(DirectoryOpenError::Path),
            None => return CandidateOpen::Skipped(SkillDiagnosticCause::Unreadable),
        },
    };
    let directory = match opened {
        Ok(directory) => directory,
        Err(DirectoryOpenError::Path(PathError::FileNotFound)) => return CandidateOpen::Missing,
        Err(_) => return CandidateOpen::Skipped(SkillDiagnosticCause::Unreadable),
    };
    let candidate = SkillCandidate {
        directory: &directory,
        path: &skill.path,
        read_authority: skill.read_authority.as_deref(),
    };
    let skill_file = match open_primary_skill_file(&candidate, authorities) {
        PrimarySkillFile::Opened(file) => file,
        PrimarySkillFile::Missing => return CandidateOpen::Missing,
        PrimarySkillFile::Rejected => {
            return CandidateOpen::Skipped(SkillDiagnosticCause::Unreadable);
        }
    };
    match inspect_skill_file(&skill_file, candidate_name) {
        Inspection::Valid(metadata, freshness) if metadata.name == skill.name => {
            CandidateOpen::Current(OpenedSkillCandidate {
                directory,
                skill_file,
                freshness,
            })
        }
        Inspection::Valid(..) => CandidateOpen::NameMismatch,
        Inspection::Invalid(cause) => {
            CandidateOpen::Skipped(SkillDiagnosticCause::InvalidMetadata(cause))
        }
        Inspection::Unreadable => CandidateOpen::Skipped(SkillDiagnosticCause::Unreadable),
        Inspection::Oversized => CandidateOpen::Skipped(SkillDiagnosticCause::Oversized),
    }
}
