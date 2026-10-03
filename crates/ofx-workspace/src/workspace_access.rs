use ofx_config::{
    DirectoryError, MAX_ADDITIONAL_DIRECTORIES, canonical_existing_directory,
    resolve_absolute_input, resolve_saved_directory,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSource {
    pub source: String,
    pub identity: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceAccess {
    entries: Vec<Entry>,
    saved_sources: Vec<SavedSource>,
}

impl WorkspaceAccess {
    pub fn new(primary: &str, saved: &[String]) -> Result<Self, DirectoryError> {
        let mut access = Self::default();
        for source in saved {
            let resolved = resolve_saved_directory(primary, source)?;
            access.merge(resolved.path.clone(), resolved.available)?;
            access.saved_sources.push(SavedSource {
                source: source.clone(),
                identity: resolved.path,
            });
        }
        Ok(access)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn saved_sources(&self) -> &[SavedSource] {
        &self.saved_sources
    }

    pub fn saved_directories(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect()
    }

    pub fn stage_add_saved(&self, primary: &str, input: &str) -> Result<Self, DirectoryError> {
        let canonical = canonical_existing_directory(primary, input)?;
        let mut replacement = self.clone();
        if self.entries.iter().any(|entry| entry.path == canonical) {
            return Ok(replacement);
        }
        if replacement.entries.len() >= MAX_ADDITIONAL_DIRECTORIES {
            return Err(DirectoryError::TooManyDirectories);
        }
        replacement.saved_sources.push(SavedSource {
            source: canonical.clone(),
            identity: canonical.clone(),
        });
        replacement.entries.push(Entry {
            path: canonical,
            available: true,
        });
        Ok(replacement)
    }

    pub fn stage_remove(&self, primary: &str, input: &str) -> Result<Self, DirectoryError> {
        let identity = self.removal_identity(primary, input)?;
        let mut replacement = self.clone();
        replacement.entries.retain(|entry| entry.path != identity);
        replacement
            .saved_sources
            .retain(|source| source.identity != identity);
        Ok(replacement)
    }

    pub fn removed_path(&self, staged: &Self) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| !staged.entries.iter().any(|kept| kept.path == entry.path))
            .map(|entry| entry.path.as_str())
    }

    fn removal_identity(&self, primary: &str, input: &str) -> Result<String, DirectoryError> {
        let normalized = resolve_absolute_input(primary, input)?;
        for source in &self.saved_sources {
            if resolve_absolute_input(primary, &source.source)? == normalized {
                return Ok(source.identity.clone());
            }
        }
        let resolved = resolve_saved_directory(primary, input)?;
        if self.entries.iter().any(|entry| entry.path == resolved.path) {
            Ok(resolved.path)
        } else {
            Err(DirectoryError::UnknownAdditionalDirectory)
        }
    }

    fn merge(&mut self, path: String, available: bool) -> Result<(), DirectoryError> {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
            entry.available |= available;
            return Ok(());
        }
        if self.entries.len() >= MAX_ADDITIONAL_DIRECTORIES {
            return Err(DirectoryError::TooManyDirectories);
        }
        self.entries.push(Entry { path, available });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn root() -> (tempfile::TempDir, String) {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path())
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap();
        for name in ["primary", "shared", "other"] {
            fs::create_dir_all(format!("{root}/{name}")).unwrap();
        }
        (directory, root)
    }

    #[test]
    fn saved_sources_merge_by_identity_and_keep_missing_directories_inactive() {
        let (_directory, root) = root();
        let primary = format!("{root}/primary");
        let access = WorkspaceAccess::new(
            &primary,
            &[
                format!("{root}/shared"),
                format!("{root}/shared/."),
                format!("{root}/gone"),
            ],
        )
        .unwrap();
        assert_eq!(
            access.entries(),
            [
                Entry {
                    path: format!("{root}/shared"),
                    available: true
                },
                Entry {
                    path: format!("{root}/gone"),
                    available: false
                },
            ]
        );
        assert_eq!(access.saved_sources().len(), 3);
        assert_eq!(
            WorkspaceAccess::new(&primary, &["relative".to_owned()]),
            Err(DirectoryError::InvalidPath)
        );
    }

    #[test]
    fn staging_adds_existing_directories_once_and_removes_by_any_spelling() {
        let (_directory, root) = root();
        let primary = format!("{root}/primary");
        let empty = WorkspaceAccess::default();
        let added = empty.stage_add_saved(&primary, "../shared").unwrap();
        assert_eq!(added.saved_directories(), [format!("{root}/shared")]);
        assert_eq!(
            added.stage_add_saved(&primary, "../shared/.").unwrap(),
            added
        );
        assert_eq!(
            empty.stage_add_saved(&primary, "../missing"),
            Err(DirectoryError::PathNotFound)
        );
        let saved = WorkspaceAccess::new(
            &primary,
            &[format!("{root}/shared"), format!("{root}/other")],
        )
        .unwrap();
        let removed = saved.stage_remove(&primary, "../shared").unwrap();
        assert_eq!(removed.saved_directories(), [format!("{root}/other")]);
        assert_eq!(
            saved.removed_path(&removed),
            Some(format!("{root}/shared").as_str())
        );
        assert_eq!(
            removed.stage_remove(&primary, &format!("{root}/shared")),
            Err(DirectoryError::UnknownAdditionalDirectory)
        );
        assert_eq!(
            removed.stage_remove(&primary, "../shared"),
            Err(DirectoryError::InvalidPath)
        );
    }

    #[test]
    fn staging_refuses_a_seventeenth_directory() {
        let (_directory, root) = root();
        let primary = format!("{root}/primary");
        let saved: Vec<String> = (0..MAX_ADDITIONAL_DIRECTORIES)
            .map(|index| format!("{root}/d{index}"))
            .collect();
        let full = WorkspaceAccess::new(&primary, &saved).unwrap();
        assert_eq!(
            full.stage_add_saved(&primary, "../shared"),
            Err(DirectoryError::TooManyDirectories)
        );
    }
}
