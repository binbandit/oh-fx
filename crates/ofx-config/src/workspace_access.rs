use std::fs;
use std::io;
use std::path::Path;

use rustix::io::Errno;
use serde_json::Value;

pub const MAX_ADDITIONAL_DIRECTORIES: usize = 16;
#[cfg(target_os = "macos")]
const MAX_PATH_BYTES: usize = 1024;
#[cfg(not(target_os = "macos"))]
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DirectoryError {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedDirectory {
    pub path: String,
    pub available: bool,
}

pub fn canonical_existing_directory(primary: &str, input: &str) -> Result<String, DirectoryError> {
    validate_input(input)?;
    let absolute = if input.starts_with('/') {
        input.to_owned()
    } else {
        resolve_lexically(&[primary, input])
    };
    let canonical = fs::canonicalize(&absolute).map_err(|error| match errno(&error) {
        Some(
            Errno::NOENT
            | Errno::ACCESS
            | Errno::PERM
            | Errno::LOOP
            | Errno::NAMETOOLONG
            | Errno::IO,
        ) => DirectoryError::PathNotFound,
        Some(Errno::NOTDIR) => DirectoryError::NotDirectory,
        _ => DirectoryError::InvalidPath,
    })?;
    let canonical = canonical
        .into_os_string()
        .into_string()
        .map_err(|_| DirectoryError::InvalidPath)?;
    let metadata = fs::symlink_metadata(&canonical).map_err(|error| match errno(&error) {
        Some(Errno::NOENT) => DirectoryError::PathNotFound,
        Some(Errno::NOTDIR) => DirectoryError::NotDirectory,
        _ => DirectoryError::InvalidPath,
    })?;
    if !metadata.is_dir() {
        return Err(DirectoryError::NotDirectory);
    }
    if canonical == primary {
        return Err(DirectoryError::PrimaryDirectory);
    }
    Ok(canonical)
}

pub fn resolve_saved_directory(
    primary: &str,
    input: &str,
) -> Result<SavedDirectory, DirectoryError> {
    if !input.starts_with('/') {
        return Err(DirectoryError::InvalidPath);
    }
    match canonical_existing_directory(primary, input) {
        Ok(path) => Ok(SavedDirectory {
            path,
            available: true,
        }),
        Err(DirectoryError::PathNotFound | DirectoryError::NotDirectory) => {
            let normalized = resolve_absolute_input(primary, input)?;
            if normalized == primary {
                return Err(DirectoryError::PrimaryDirectory);
            }
            Ok(SavedDirectory {
                path: nearest_existing_identity(&normalized)?,
                available: false,
            })
        }
        Err(error) => Err(error),
    }
}

pub fn resolve_absolute_input(primary: &str, input: &str) -> Result<String, DirectoryError> {
    validate_input(input)?;
    Ok(resolve_lexically(&[primary, input]))
}

pub(crate) fn parse_sources(primary: &Path, value: &Value) -> Option<Vec<String>> {
    let primary = primary.to_str()?;
    let mut sources: Vec<String> = Vec::new();
    for item in value.as_array()? {
        let source = item.as_str()?;
        if sources.len() >= MAX_ADDITIONAL_DIRECTORIES || sources.iter().any(|seen| seen == source)
        {
            return None;
        }
        sources.push(source.to_owned());
    }
    for source in &sources {
        resolve_saved_directory(primary, source).ok()?;
    }
    Some(sources)
}

fn validate_input(input: &str) -> Result<(), DirectoryError> {
    if input.is_empty() || input.len() > MAX_PATH_BYTES || input.contains('\0') {
        Err(DirectoryError::InvalidPath)
    } else {
        Ok(())
    }
}

fn nearest_existing_identity(absolute: &str) -> Result<String, DirectoryError> {
    let mut missing: Vec<&str> = Vec::new();
    let mut current = absolute;
    loop {
        match fs::canonicalize(current) {
            Ok(existing) => {
                if !missing.is_empty() && !existing.is_dir() {
                    return Err(DirectoryError::InvalidPath);
                }
                let mut resolved = existing
                    .into_os_string()
                    .into_string()
                    .map_err(|_| DirectoryError::InvalidPath)?;
                for component in missing.iter().rev() {
                    if !resolved.ends_with('/') {
                        resolved.push('/');
                    }
                    resolved.push_str(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (parent, name) = current
                    .rsplit_once('/')
                    .filter(|(_, name)| !name.is_empty())
                    .ok_or(DirectoryError::InvalidPath)?;
                missing.push(name);
                current = if parent.is_empty() { "/" } else { parent };
            }
            Err(_) => return Err(DirectoryError::InvalidPath),
        }
    }
}

fn resolve_lexically(paths: &[&str]) -> String {
    let mut components: Vec<&str> = Vec::new();
    for path in paths {
        if path.starts_with('/') {
            components.clear();
        }
        for component in path.split('/') {
            match component {
                "" | "." => {}
                ".." => {
                    components.pop();
                }
                name => components.push(name),
            }
        }
    }
    format!("/{}", components.join("/"))
}

fn errno(error: &io::Error) -> Option<Errno> {
    error.raw_os_error().map(Errno::from_raw_os_error)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn root() -> (tempfile::TempDir, String) {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path())
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap();
        fs::create_dir_all(format!("{root}/primary")).unwrap();
        fs::create_dir_all(format!("{root}/shared")).unwrap();
        fs::write(format!("{root}/file"), "x").unwrap();
        (directory, root)
    }

    #[test]
    fn existing_directories_resolve_to_their_real_path() {
        let (_directory, root) = root();
        let primary = format!("{root}/primary");
        symlink(format!("{root}/shared"), format!("{root}/link")).unwrap();
        for input in [
            format!("{root}/shared"),
            format!("{root}/link"),
            "../shared".to_owned(),
            format!("{root}/primary/../shared/."),
        ] {
            assert_eq!(
                canonical_existing_directory(&primary, &input),
                Ok(format!("{root}/shared")),
                "{input}"
            );
        }
        assert_eq!(
            canonical_existing_directory(&primary, "."),
            Err(DirectoryError::PrimaryDirectory)
        );
        assert_eq!(
            canonical_existing_directory(&primary, "../missing"),
            Err(DirectoryError::PathNotFound)
        );
        assert_eq!(
            canonical_existing_directory(&primary, "../file"),
            Err(DirectoryError::NotDirectory)
        );
        assert_eq!(
            canonical_existing_directory(&primary, ""),
            Err(DirectoryError::InvalidPath)
        );
        assert_eq!(
            canonical_existing_directory(&primary, "a\0b"),
            Err(DirectoryError::InvalidPath)
        );
    }

    #[test]
    fn saved_directories_must_be_absolute_and_may_be_missing() {
        let (_directory, root) = root();
        let primary = format!("{root}/primary");
        assert_eq!(
            resolve_saved_directory(&primary, &format!("{root}/shared")),
            Ok(SavedDirectory {
                path: format!("{root}/shared"),
                available: true
            })
        );
        assert_eq!(
            resolve_saved_directory(&primary, &format!("{root}/gone/./deeper/../child")),
            Ok(SavedDirectory {
                path: format!("{root}/gone/child"),
                available: false
            })
        );
        assert_eq!(
            resolve_saved_directory(&primary, &format!("{root}/file")),
            Ok(SavedDirectory {
                path: format!("{root}/file"),
                available: false
            })
        );
        assert_eq!(
            resolve_saved_directory(&primary, &format!("{root}/file/below")),
            Err(DirectoryError::InvalidPath)
        );
        assert_eq!(
            resolve_saved_directory(&primary, "shared"),
            Err(DirectoryError::InvalidPath)
        );
        assert_eq!(
            resolve_saved_directory(&primary, &format!("{root}/shared/../primary")),
            Err(DirectoryError::PrimaryDirectory)
        );
    }

    #[test]
    fn saved_sources_must_be_unique_absolute_strings_within_the_limit() {
        let (_directory, root) = root();
        let primary = Path::new(&root).join("primary");
        let shared = format!("{root}/shared");
        let parse = |value: Value| parse_sources(&primary, &value);
        assert_eq!(
            parse(serde_json::json!([shared, format!("{root}/missing")])),
            Some(vec![shared.clone(), format!("{root}/missing")])
        );
        assert_eq!(
            parse(serde_json::json!([shared, format!("{root}/shared/.")])),
            Some(vec![shared.clone(), format!("{root}/shared/.")])
        );
        assert_eq!(parse(serde_json::json!([shared, shared])), None);
        assert_eq!(parse(serde_json::json!(["relative"])), None);
        assert_eq!(parse(serde_json::json!([7])), None);
        assert_eq!(parse(serde_json::json!(shared)), None);
        assert_eq!(parse(serde_json::json!([primary])), None);
        let many: Vec<String> = (0..17).map(|index| format!("{root}/d{index}")).collect();
        assert_eq!(parse(serde_json::json!(many)), None);
    }
}
