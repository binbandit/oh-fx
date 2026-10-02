use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ofx_workspace::{
    FileIdentity, FileKind, PathError, dirname, entry_identity, open_child_directory,
    open_directory, path_inside,
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

struct SkillRoot<'a> {
    path: PathBuf,
    declared_from: usize,
    source: SkillSource,
    read_authority: Option<&'a Path>,
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

    fn roots(&self, policy: &RootPolicy) -> Vec<SkillRoot<'_>> {
        let mut roots =
            Vec::with_capacity(policy.workspace_roots.len() + 1 + policy.global_roots.len());
        if let Some(workspace_root) = &self.workspace_root {
            self.append_workspace_roots(&mut roots, workspace_root, policy.workspace_roots);
        }
        if let Some(source) = policy.managed_root_source {
            let parent = dirname(self.managed_root.as_os_str().as_bytes()).map_or(0, <[u8]>::len);
            push_root(&mut roots, self.managed_root.clone(), parent, source, None);
        }
        if let Some(home) = &self.home {
            for spec in policy.global_roots {
                push_spec_root(&mut roots, home, spec);
            }
        }
        roots
    }

    fn append_workspace_roots<'a>(
        &'a self,
        roots: &mut Vec<SkillRoot<'a>>,
        workspace_root: &'a Path,
        specs: &[RootSpec],
    ) {
        let home = self.home.as_deref();
        let mut current = Some(workspace_root);
        while let Some(directory) = current {
            if home == Some(directory) {
                break;
            }
            for spec in specs {
                push_spec_root(roots, directory, spec);
            }
            current = home
                .filter(|home| path_inside(home, directory))
                .and_then(|_| dirname(directory.as_os_str().as_bytes()))
                .map(|parent| Path::new(OsStr::from_bytes(parent)));
        }
    }
}

fn push_spec_root<'a>(roots: &mut Vec<SkillRoot<'a>>, base: &'a Path, spec: &RootSpec) {
    let mut path = PathBuf::with_capacity(base.as_os_str().len() + 1 + spec.path.len());
    path.push(base);
    path.push(spec.path);
    let declared_from = base.as_os_str().len();
    push_root(roots, path, declared_from, spec.source, Some(base));
}

fn push_root<'a>(
    roots: &mut Vec<SkillRoot<'a>>,
    path: PathBuf,
    declared_from: usize,
    source: SkillSource,
    read_authority: Option<&'a Path>,
) {
    if roots
        .iter()
        .any(|root| root.path.as_os_str() == path.as_os_str())
    {
        return;
    }
    roots.push(SkillRoot {
        path,
        declared_from,
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

    fn diagnose_unreadable_root(&mut self, root: &SkillRoot<'_>) {
        self.diagnose(
            root.path.clone(),
            root.source,
            SkillDiagnosticScope::Root,
            SkillDiagnosticCause::Unreadable,
        );
    }

    fn append_root(&mut self, root: &SkillRoot<'_>) {
        if root.is_missing() {
            return;
        }
        let opened = match root.read_authority {
            Some(authority) => open_contained_directory(&root.path, authority, self.authorities),
            None => open_directory(&root.path).map_err(DirectoryOpenError::Path),
        };
        let directory = match opened {
            Ok(directory) => directory,
            Err(error) => {
                if !(error.is_missing() && root.is_missing()) {
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
        root: &SkillRoot<'_>,
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

    fn open_linked_candidate(&mut self, root: &SkillRoot<'_>, name: &OsStr) -> Option<OwnedFd> {
        let authority = root.read_authority?;
        let path = root.path.join(name);
        let opened = open_contained_directory(&path, authority, self.authorities);
        if opened.is_err() {
            self.diagnose_candidate(root, path, SkillDiagnosticCause::LinkedCandidateUnavailable);
        }
        opened.ok()
    }

    fn append_candidate(&mut self, root: &SkillRoot<'_>, candidate: &OwnedFd, name: &OsStr) {
        let path = root.path.join(name);
        let candidate = SkillCandidate {
            directory: candidate,
            path: &path,
            read_authority: root.read_authority,
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
                    read_authority: root.read_authority.map(Path::to_path_buf),
                });
                return;
            }
            Inspection::Invalid(cause) => SkillDiagnosticCause::InvalidMetadata(cause),
            Inspection::Unreadable => SkillDiagnosticCause::Unreadable,
            Inspection::Oversized => SkillDiagnosticCause::Oversized,
        };
        self.diagnose_candidate(root, path, cause);
    }

    fn diagnose_candidate(
        &mut self,
        root: &SkillRoot<'_>,
        path: PathBuf,
        cause: SkillDiagnosticCause,
    ) {
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

impl SkillRoot<'_> {
    fn is_missing(&self) -> bool {
        let bytes = self.path.as_os_str().as_bytes();
        if bytes.first() != Some(&b'/') {
            return false;
        }
        let mut end = self.declared_from;
        for name in bytes[self.declared_from..].split(|&byte| byte == b'/') {
            end += name.len() + 1;
            match name {
                b"" | b"." => continue,
                b".." => return false,
                _ => {}
            }
            let prefix = OsStr::from_bytes(&bytes[..end - 1]);
            match entry_identity(CWD, prefix).map(FileIdentity::kind) {
                Err(error) => return error == PathError::FileNotFound,
                Ok(FileKind::Directory) => {}
                Ok(_) => return false,
            }
        }
        false
    }
}

#[cfg(test)]
mod tests;
