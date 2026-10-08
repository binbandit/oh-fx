use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::path_error::PathError;
use crate::pathing::{MAX_PATH_BYTES, path_inside, resolve_lexically};

pub const MAX_ADDITIONAL_DIRECTORIES: usize = 16;
const SAVED: DirectorySource = DirectorySource {
    saved: true,
    command_line: false,
};
const COMMAND_LINE: DirectorySource = DirectorySource {
    saved: false,
    command_line: true,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceAccessError {
    #[error("InvalidPath")]
    InvalidPath,
    #[error("PathNotFound")]
    PathNotFound,
    #[error("NotDirectory")]
    NotDirectory,
    #[error("UnknownAdditionalDirectory")]
    UnknownAdditionalDirectory,
    #[error("PrimaryDirectory")]
    PrimaryDirectory,
    #[error("TooManyDirectories")]
    TooManyDirectories,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectorySource {
    pub saved: bool,
    pub command_line: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditionalDirectory {
    pub path: PathBuf,
    pub source: DirectorySource,
    pub available: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedSource {
    pub(crate) source: String,
    pub(crate) identity: PathBuf,
    pub(crate) identity_canonical: bool,
}

struct RefreshedSource<'a> {
    previous: &'a Path,
    saved: SavedSource,
    available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceAccess {
    primary: PathBuf,
    entries: Vec<AdditionalDirectory>,
    saved_sources: Vec<SavedSource>,
    saved_suppressed: bool,
}

impl WorkspaceAccess {
    pub fn new(primary: &Path, saved: &[String]) -> Result<Self, WorkspaceAccessError> {
        let mut access = Self::primary_only(primary);
        for path in saved {
            let (identity, available) = resolve_saved_directory(primary, path)?;
            access.merge(identity.clone(), SAVED, available)?;
            access.saved_sources.push(SavedSource {
                source: path.clone(),
                identity,
                identity_canonical: available,
            });
        }
        access.recompute_active();
        Ok(access)
    }

    pub fn primary_only(primary: &Path) -> Self {
        Self {
            primary: primary.to_path_buf(),
            entries: Vec::new(),
            saved_sources: Vec::new(),
            saved_suppressed: false,
        }
    }

    pub fn apply_launch(
        &self,
        command_line: &[OsString],
        saved_suppressed: bool,
    ) -> Result<Self, WorkspaceAccessError> {
        let mut replacement = Self {
            primary: self.primary.clone(),
            entries: self
                .entries
                .iter()
                .filter(|entry| entry.source.saved)
                .map(|entry| AdditionalDirectory {
                    source: SAVED,
                    ..entry.clone()
                })
                .collect(),
            saved_sources: self.saved_sources.clone(),
            saved_suppressed,
        };
        for path in command_line {
            let path = path.to_str().ok_or(WorkspaceAccessError::InvalidPath)?;
            let canonical = canonical_existing_directory(&replacement.primary, path)?;
            replacement.merge(canonical, COMMAND_LINE, true)?;
        }
        replacement.recompute_active();
        Ok(replacement)
    }

    pub fn entries(&self) -> &[AdditionalDirectory] {
        &self.entries
    }

    pub fn active_roots(&self) -> impl Iterator<Item = &Path> {
        self.entries
            .iter()
            .filter(|entry| entry.active)
            .map(|entry| entry.path.as_path())
    }

    pub fn additional_root_for(&self, path: &Path) -> Option<&Path> {
        self.active_roots().find(|root| path_inside(root, path))
    }

    pub fn saved_suppressed(&self) -> bool {
        self.saved_suppressed
    }

    pub fn primary(&self) -> &Path {
        &self.primary
    }

    pub(crate) fn saved_sources(&self) -> &[SavedSource] {
        &self.saved_sources
    }

    pub(crate) fn saved_directories(&self) -> impl Iterator<Item = &Path> {
        self.entries
            .iter()
            .filter(|entry| entry.source.saved)
            .map(|entry| entry.path.as_path())
    }

    pub(crate) fn command_line_directories(&self) -> impl Iterator<Item = &Path> {
        self.entries
            .iter()
            .filter(|entry| entry.source.command_line)
            .map(|entry| entry.path.as_path())
    }

    pub(crate) fn command_line_source_removed(&self, replacement: &Self) -> bool {
        self.command_line_directories().any(|path| {
            !replacement
                .command_line_directories()
                .any(|kept| kept == path)
        })
    }

    pub(crate) fn add_directory_identity(
        &self,
        input: &str,
    ) -> Result<PathBuf, WorkspaceAccessError> {
        canonical_existing_directory(&self.primary, input)
    }

    pub(crate) fn stage_add_saved(&self, input: &str) -> Result<Self, WorkspaceAccessError> {
        let canonical = canonical_existing_directory(&self.primary, input)?;
        let mut replacement = self.clone();
        if self
            .entries
            .iter()
            .any(|entry| entry.source.saved && entry.path == canonical)
        {
            return Ok(replacement);
        }
        replacement.saved_sources.push(SavedSource {
            source: canonical
                .to_str()
                .ok_or(WorkspaceAccessError::InvalidPath)?
                .to_owned(),
            identity: canonical.clone(),
            identity_canonical: true,
        });
        replacement.merge(canonical, SAVED, true)?;
        replacement.recompute_active();
        Ok(replacement)
    }

    pub(crate) fn stage_remove(&self, input: &str) -> Result<Self, WorkspaceAccessError> {
        let identity = self.removal_identity(input)?;
        let mut replacement = self.clone();
        replacement.entries.retain(|entry| entry.path != identity);
        replacement
            .saved_sources
            .retain(|source| source.identity != identity);
        replacement.recompute_active();
        Ok(replacement)
    }

    pub fn stage_availability_refresh(&self) -> Result<Option<Self>, WorkspaceAccessError> {
        let refreshed = self
            .saved_sources
            .iter()
            .map(|source| self.refreshed_source(source))
            .collect::<Result<Vec<_>, _>>()?;
        let mut replacement = Self {
            saved_sources: refreshed
                .iter()
                .map(|source| source.saved.clone())
                .collect(),
            saved_suppressed: self.saved_suppressed,
            ..Self::primary_only(&self.primary)
        };
        let mut appended = vec![false; refreshed.len()];
        for entry in &self.entries {
            for (source, appended) in refreshed.iter().zip(&mut appended) {
                if *appended || source.previous != entry.path {
                    continue;
                }
                replacement.merge(source.saved.identity.clone(), SAVED, source.available)?;
                *appended = true;
            }
            if entry.source.command_line {
                let (path, available) = resolve_observed_directory(&self.primary, &entry.path)?;
                replacement.merge(path, COMMAND_LINE, available)?;
            }
        }
        for (source, appended) in refreshed.iter().zip(appended) {
            if !appended {
                replacement.merge(source.saved.identity.clone(), SAVED, source.available)?;
            }
        }
        replacement.recompute_active();
        Ok((replacement != *self).then_some(replacement))
    }

    fn refreshed_source<'a>(
        &self,
        source: &'a SavedSource,
    ) -> Result<RefreshedSource<'a>, WorkspaceAccessError> {
        let (identity, available) = if source.identity_canonical {
            resolve_observed_directory(&self.primary, &source.identity)?
        } else {
            resolve_saved_directory(&self.primary, &source.source)?
        };
        Ok(RefreshedSource {
            previous: &source.identity,
            saved: SavedSource {
                source: source.source.clone(),
                identity,
                identity_canonical: source.identity_canonical || available,
            },
            available,
        })
    }

    pub(crate) fn stage_clear(&self) -> Self {
        Self {
            saved_suppressed: self.saved_suppressed,
            ..Self::primary_only(&self.primary)
        }
    }

    fn removal_identity(&self, input: &str) -> Result<PathBuf, WorkspaceAccessError> {
        let normalized = resolve_absolute_input(&self.primary, input)?;
        for source in &self.saved_sources {
            if resolve_absolute_input(&self.primary, &source.source)? == normalized {
                return Ok(source.identity.clone());
            }
        }
        let (identity, _) = resolve_saved_directory(&self.primary, input)?;
        if self.entries.iter().any(|entry| entry.path == identity) {
            Ok(identity)
        } else {
            Err(WorkspaceAccessError::UnknownAdditionalDirectory)
        }
    }

    fn merge(
        &mut self,
        path: PathBuf,
        source: DirectorySource,
        available: bool,
    ) -> Result<(), WorkspaceAccessError> {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
            entry.source.saved |= source.saved;
            entry.source.command_line |= source.command_line;
            entry.available |= available;
            return Ok(());
        }
        if self.entries.len() >= MAX_ADDITIONAL_DIRECTORIES {
            return Err(WorkspaceAccessError::TooManyDirectories);
        }
        self.entries.push(AdditionalDirectory {
            path,
            source,
            available,
            active: false,
        });
        Ok(())
    }

    fn recompute_active(&mut self) {
        for entry in &mut self.entries {
            let source_active =
                entry.source.command_line || (entry.source.saved && !self.saved_suppressed);
            entry.active = entry.available && source_active;
        }
    }
}

fn resolve_saved_directory(
    primary: &Path,
    input: &str,
) -> Result<(PathBuf, bool), WorkspaceAccessError> {
    if !input.starts_with('/') {
        return Err(WorkspaceAccessError::InvalidPath);
    }
    match canonical_existing_directory(primary, input) {
        Ok(canonical) => Ok((canonical, true)),
        Err(WorkspaceAccessError::PathNotFound | WorkspaceAccessError::NotDirectory) => {
            let normalized = bytes_path(&resolve_lexically(&[input.as_bytes()])).to_path_buf();
            if normalized == primary {
                return Err(WorkspaceAccessError::PrimaryDirectory);
            }
            let identity = resolve_from_nearest_existing(&normalized)
                .ok_or(WorkspaceAccessError::InvalidPath)?;
            Ok((identity, false))
        }
        Err(error) => Err(error),
    }
}

fn resolve_observed_directory(
    primary: &Path,
    identity: &Path,
) -> Result<(PathBuf, bool), WorkspaceAccessError> {
    let text = identity.to_str().ok_or(WorkspaceAccessError::InvalidPath)?;
    match canonical_existing_directory(primary, text) {
        Ok(canonical) => Ok((identity.to_path_buf(), canonical == identity)),
        Err(WorkspaceAccessError::PathNotFound | WorkspaceAccessError::NotDirectory) => {
            Ok((identity.to_path_buf(), false))
        }
        Err(error) => Err(error),
    }
}

fn resolve_absolute_input(primary: &Path, input: &str) -> Result<PathBuf, WorkspaceAccessError> {
    let input = checked_input(input)?;
    let resolved = if input.starts_with('/') {
        resolve_lexically(&[input.as_bytes()])
    } else {
        resolve_lexically(&[primary.as_os_str().as_bytes(), input.as_bytes()])
    };
    Ok(bytes_path(&resolved).to_path_buf())
}

fn checked_input(input: &str) -> Result<&str, WorkspaceAccessError> {
    if input.is_empty() || input.len() > MAX_PATH_BYTES || input.contains('\0') {
        return Err(WorkspaceAccessError::InvalidPath);
    }
    Ok(input)
}

fn canonical_existing_directory(
    primary: &Path,
    input: &str,
) -> Result<PathBuf, WorkspaceAccessError> {
    let input = checked_input(input)?;
    let absolute = if input.starts_with('/') {
        input.as_bytes().to_vec()
    } else {
        resolve_lexically(&[primary.as_os_str().as_bytes(), input.as_bytes()])
    };
    let canonical =
        fs::canonicalize(bytes_path(&absolute)).map_err(|error| match PathError::from_realpath(
            &error,
        ) {
            PathError::FileNotFound
            | PathError::AccessDenied
            | PathError::PermissionDenied
            | PathError::SymLinkLoop
            | PathError::NameTooLong
            | PathError::InputOutput => WorkspaceAccessError::PathNotFound,
            PathError::NotDir => WorkspaceAccessError::NotDirectory,
            _ => WorkspaceAccessError::InvalidPath,
        })?;
    let metadata =
        fs::symlink_metadata(&canonical).map_err(|error| {
            match PathError::from_realpath(&error) {
                PathError::FileNotFound => WorkspaceAccessError::PathNotFound,
                PathError::NotDir => WorkspaceAccessError::NotDirectory,
                _ => WorkspaceAccessError::InvalidPath,
            }
        })?;
    if !metadata.is_dir() {
        return Err(WorkspaceAccessError::NotDirectory);
    }
    if canonical == primary {
        return Err(WorkspaceAccessError::PrimaryDirectory);
    }
    Ok(canonical)
}

fn resolve_from_nearest_existing(absolute: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut current = absolute;
    loop {
        match fs::canonicalize(current) {
            Ok(existing) => {
                if !missing.is_empty() && !existing.is_dir() {
                    return None;
                }
                return Some(
                    missing
                        .iter()
                        .rev()
                        .fold(existing, |path, name| path.join(name)),
                );
            }
            Err(error) if PathError::from_realpath(&error) == PathError::FileNotFound => {
                missing.push(current.file_name()?.to_owned());
                current = current.parent()?;
            }
            Err(_) => return None,
        }
    }
}

fn bytes_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

#[cfg(test)]
mod tests;
