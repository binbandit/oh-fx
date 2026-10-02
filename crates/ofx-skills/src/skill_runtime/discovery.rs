use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use ofx_workspace::{
    FileIdentity, FileKind, PathError, dirname, entry_identity, open_child_directory,
    open_directory,
};
use rustix::fs::{CWD, Dir, FileType, Mode, OFlags, openat};

use super::SymlinkAuthorities;
use super::skill_file::{
    DirectoryOpenError, Inspection, PrimarySkillFile, SkillCandidate, inspect_skill_file,
    open_contained_directory, open_primary_skill_file,
};
use crate::skill_contract::{
    RootPolicy, RootSpec, Skill, SkillDiagnostic, SkillDiagnosticCause, SkillDiagnosticScope,
    SkillSource,
};

const LISTING_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiscoveryContext {
    pub workspace_root: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub managed_root: PathBuf,
    pub symlink_authorities: SymlinkAuthorities,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillDiscovery {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

struct SkillRoot {
    path: PathBuf,
    source: SkillSource,
    read_authority: Option<PathBuf>,
}

impl SkillDiscoveryContext {
    pub fn load_visible_skills(&self, policy: &RootPolicy) -> SkillDiscovery {
        let mut scan = DiscoveryScan {
            authorities: &self.symlink_authorities,
            discovery: SkillDiscovery::default(),
            canonical_paths: HashSet::new(),
        };
        for root in self.roots(policy) {
            scan.append_root(&root);
        }
        scan.discovery
    }

    fn roots(&self, policy: &RootPolicy) -> Vec<SkillRoot> {
        let mut roots = Vec::new();
        if let Some(workspace_root) = &self.workspace_root {
            self.append_workspace_roots(&mut roots, workspace_root, policy.workspace_roots);
        }
        if let Some(source) = policy.managed_root_source {
            push_root(&mut roots, self.managed_root.clone(), source, None);
        }
        if let Some(home) = &self.home {
            for spec in policy.global_roots {
                push_spec_root(&mut roots, home, spec);
            }
        }
        roots
    }

    fn append_workspace_roots(
        &self,
        roots: &mut Vec<SkillRoot>,
        workspace_root: &Path,
        specs: &[RootSpec],
    ) {
        let home = self.home.as_deref().map(Path::as_os_str);
        let mut current = Some(workspace_root.as_os_str().as_bytes());
        while let Some(directory) = current {
            let directory = OsStr::from_bytes(directory);
            if home == Some(directory) {
                break;
            }
            for spec in specs {
                push_spec_root(roots, Path::new(directory), spec);
            }
            current = dirname(directory.as_bytes());
        }
    }
}

fn push_spec_root(roots: &mut Vec<SkillRoot>, base: &Path, spec: &RootSpec) {
    push_root(
        roots,
        base.join(spec.path),
        spec.source,
        Some(base.to_path_buf()),
    );
}

fn push_root(
    roots: &mut Vec<SkillRoot>,
    path: PathBuf,
    source: SkillSource,
    read_authority: Option<PathBuf>,
) {
    if roots
        .iter()
        .any(|root| root.path.as_os_str() == path.as_os_str())
    {
        return;
    }
    roots.push(SkillRoot {
        path,
        source,
        read_authority,
    });
}

struct DiscoveryScan<'a> {
    authorities: &'a SymlinkAuthorities,
    discovery: SkillDiscovery,
    canonical_paths: HashSet<PathBuf>,
}

struct CandidateEntry {
    name: Vec<u8>,
    linked: bool,
}

impl DiscoveryScan<'_> {
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
        let opened = match &root.read_authority {
            Some(authority) => open_contained_directory(&root.path, authority, self.authorities),
            None => open_directory(&root.path).map_err(DirectoryOpenError::Path),
        };
        let directory = match opened {
            Ok(directory) => directory,
            Err(error) => {
                if !(error.is_missing() && root_path_is_missing(&root.path)) {
                    self.diagnose_unreadable_root(root);
                }
                return;
            }
        };
        let Ok(entries) = candidate_entries(&directory, root.read_authority.is_some()) else {
            self.diagnose_unreadable_root(root);
            return;
        };
        for entry in entries {
            let name = OsStr::from_bytes(&entry.name);
            let candidate = if entry.linked {
                self.open_linked_candidate(root, name)
            } else {
                self.open_candidate(root, &directory, name)
            };
            if let Some(candidate) = candidate {
                self.append_candidate(root, &candidate, name);
            }
        }
    }

    fn open_candidate(
        &mut self,
        root: &SkillRoot,
        root_directory: &OwnedFd,
        name: &OsStr,
    ) -> Option<OwnedFd> {
        match open_child_directory(root_directory, name) {
            Ok(candidate) => Some(candidate),
            Err(PathError::FileNotFound) => None,
            Err(_) => {
                self.diagnose_candidate(
                    root,
                    root.path.join(name),
                    SkillDiagnosticCause::Unreadable,
                );
                None
            }
        }
    }

    fn open_linked_candidate(&mut self, root: &SkillRoot, name: &OsStr) -> Option<OwnedFd> {
        let authority = root.read_authority.as_deref()?;
        let path = root.path.join(name);
        let opened = open_contained_directory(&path, authority, self.authorities);
        if opened.is_err() {
            self.diagnose_candidate(root, path, SkillDiagnosticCause::LinkedCandidateUnavailable);
        }
        opened.ok()
    }

    fn append_candidate(&mut self, root: &SkillRoot, candidate: &OwnedFd, name: &OsStr) {
        let path = root.path.join(name);
        let candidate = SkillCandidate {
            directory: candidate,
            path: &path,
            read_authority: root.read_authority.as_deref(),
        };
        let file = match open_primary_skill_file(&candidate, self.authorities) {
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
            Inspection::Valid(metadata, _) => {
                self.discovery.skills.push(Skill {
                    name: metadata.name,
                    description: metadata.description,
                    path,
                    source: root.source,
                    read_authority: root.read_authority.clone(),
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

fn candidate_entries(directory: &OwnedFd, allow_linked: bool) -> io::Result<Vec<CandidateEntry>> {
    let listing = openat(directory, ".", LISTING_FLAGS, Mode::empty())?;
    let mut entries = Vec::new();
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
            FileType::Symlink => Some(FileKind::Symlink),
            _ => None,
        };
        let linked = kind == Some(FileKind::Symlink);
        if kind == Some(FileKind::Directory) || (linked && allow_linked) {
            entries.push(CandidateEntry {
                name: name.to_vec(),
                linked,
            });
        }
    }
    entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
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
