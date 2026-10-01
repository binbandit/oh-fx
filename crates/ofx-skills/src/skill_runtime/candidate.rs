use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_workspace::{PathError, basename, dirname, open_child_directory, open_directory};

use super::SymlinkAuthorities;
use super::skill_file::{
    DirectoryOpenError, Inspection, PrimarySkillFile, SkillCandidate, inspect_skill_file,
    open_contained_directory, open_primary_skill_file,
};
use crate::skill_contract::{Skill, SkillDiagnosticCause};

#[derive(Debug)]
pub(crate) struct OpenedSkillCandidate {
    skill_file: File,
}

#[derive(Debug)]
pub(crate) enum CandidateOpen {
    Current(OpenedSkillCandidate),
    Missing,
    NameMismatch,
    Skipped(SkillDiagnosticCause),
}

impl OpenedSkillCandidate {
    pub(crate) fn skill_file(&self) -> &File {
        &self.skill_file
    }
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
        Inspection::Valid(metadata) if metadata.name == skill.name => {
            CandidateOpen::Current(OpenedSkillCandidate { skill_file })
        }
        Inspection::Valid(_) => CandidateOpen::NameMismatch,
        Inspection::Invalid(cause) => {
            CandidateOpen::Skipped(SkillDiagnosticCause::InvalidMetadata(cause))
        }
        Inspection::Unreadable => CandidateOpen::Skipped(SkillDiagnosticCause::Unreadable),
        Inspection::Oversized => CandidateOpen::Skipped(SkillDiagnosticCause::Oversized),
    }
}
