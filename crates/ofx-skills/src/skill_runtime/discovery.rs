use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use ofx_workspace::{
    FileIdentity, FileKind, PathError, entry_identity, open_child_directory, open_directory,
};
use rustix::fs::{CWD, Dir, FileType, Mode, OFlags, openat};

use super::skill_file::{
    Inspection, PrimarySkillFile, inspect_skill_file, open_primary_skill_file,
};
use crate::skill_contract::{
    RootPolicy, Skill, SkillDiagnostic, SkillDiagnosticCause, SkillDiagnosticScope, SkillSource,
};

const LISTING_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiscoveryContext {
    pub managed_root: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillDiscovery {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

struct SkillRoot {
    path: PathBuf,
    source: SkillSource,
}

impl SkillDiscoveryContext {
    pub fn load_visible_skills(&self, policy: &RootPolicy) -> SkillDiscovery {
        let mut scan = DiscoveryScan::default();
        if let Some(source) = policy.managed_root_source {
            scan.append_root(&SkillRoot {
                path: self.managed_root.clone(),
                source,
            });
        }
        scan.discovery
    }
}

#[derive(Default)]
struct DiscoveryScan {
    discovery: SkillDiscovery,
    canonical_paths: HashSet<PathBuf>,
}

impl DiscoveryScan {
    fn diagnose(
        &mut self,
        path: PathBuf,
        source: SkillSource,
        scope: SkillDiagnosticScope,
        cause: SkillDiagnosticCause,
    ) {
        self.discovery.diagnostics.push(SkillDiagnostic {
            path,
            source,
            scope,
            cause,
        });
    }

    fn diagnose_unreadable_root(&mut self, root: &SkillRoot) {
        self.diagnose(
            root.path.clone(),
            root.source,
            SkillDiagnosticScope::Root,
            SkillDiagnosticCause::Unreadable,
        );
    }

    fn append_root(&mut self, root: &SkillRoot) {
        let directory = match open_directory(&root.path) {
            Ok(directory) => directory,
            Err(error) => {
                if !(is_missing(error) && root_path_is_missing(&root.path)) {
                    self.diagnose_unreadable_root(root);
                }
                return;
            }
        };
        let Ok(names) = candidate_names(&directory) else {
            self.diagnose_unreadable_root(root);
            return;
        };
        for name in names {
            self.append_candidate(root, &directory, OsStr::from_bytes(&name));
        }
    }

    fn append_candidate(&mut self, root: &SkillRoot, root_directory: &OwnedFd, name: &OsStr) {
        let path = root.path.join(name);
        let candidate = match open_child_directory(root_directory, name) {
            Ok(candidate) => candidate,
            Err(PathError::FileNotFound) => return,
            Err(_) => {
                self.diagnose_candidate(root, path, SkillDiagnosticCause::Unreadable);
                return;
            }
        };
        let file = match open_primary_skill_file(&candidate) {
            PrimarySkillFile::Opened(file) => file,
            PrimarySkillFile::Missing => return,
            PrimarySkillFile::Rejected => {
                self.diagnose_candidate(root, path, SkillDiagnosticCause::Unreadable);
                return;
            }
        };
        if !self.remember_canonical_path(&path) {
            return;
        }
        let cause = match inspect_skill_file(&file, name.as_bytes()) {
            Inspection::Valid(metadata) => {
                self.discovery.skills.push(Skill {
                    name: metadata.name,
                    description: metadata.description,
                    path,
                    source: root.source,
                });
                return;
            }
            Inspection::Invalid(cause) => SkillDiagnosticCause::InvalidMetadata(cause),
            Inspection::Unreadable => SkillDiagnosticCause::Unreadable,
            Inspection::Oversized => SkillDiagnosticCause::Oversized,
        };
        self.diagnose_candidate(root, path, cause);
    }

    fn diagnose_candidate(&mut self, root: &SkillRoot, path: PathBuf, cause: SkillDiagnosticCause) {
        self.diagnose(path, root.source, SkillDiagnosticScope::Candidate, cause);
    }

    fn remember_canonical_path(&mut self, logical_path: &Path) -> bool {
        match fs::canonicalize(logical_path) {
            Ok(canonical) => self.canonical_paths.insert(canonical),
            Err(_) => true,
        }
    }
}

fn is_missing(error: PathError) -> bool {
    matches!(error, PathError::FileNotFound | PathError::NotDir)
}

fn candidate_names(directory: &OwnedFd) -> io::Result<Vec<Vec<u8>>> {
    let listing = openat(directory, ".", LISTING_FLAGS, Mode::empty())?;
    let mut names = Vec::new();
    for entry in Dir::new(listing)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let kind = match entry.file_type() {
            FileType::Unknown => entry_identity(directory, OsStr::from_bytes(name))
                .map(FileIdentity::kind)
                .ok(),
            FileType::Directory => Some(FileKind::Directory),
            _ => None,
        };
        if kind == Some(FileKind::Directory) {
            names.push(name.to_vec());
        }
    }
    names.sort_unstable();
    Ok(names)
}

fn root_path_is_missing(path: &Path) -> bool {
    let mut components = path.components().peekable();
    if components.next() != Some(Component::RootDir) {
        return false;
    }
    let mut current = PathBuf::from("/");
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return false;
        };
        current.push(name);
        match entry_identity(CWD, current.as_os_str()).map(FileIdentity::kind) {
            Err(error) => return error == PathError::FileNotFound,
            Ok(FileKind::Directory) if components.peek().is_some() => {}
            Ok(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests;
