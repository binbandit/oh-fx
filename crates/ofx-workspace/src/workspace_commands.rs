use std::path::Path;

use ofx_config::{
    DirectoryError, MAX_ADDITIONAL_DIRECTORIES, ProfilePaths, Settings, SettingsError,
    SettingsWriteError, WorkspaceSaveError, canonical_existing_directory, save_workspace_entry,
};
use serde_json::{Map, Value};

use crate::workspace_access::{SavedSource, WorkspaceAccess};

const DIRECTORIES_KEY: &str = "additional_directories";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Add(String),
    Remove(String),
    Clear,
}

impl Action {
    fn label(&self) -> &'static str {
        match self {
            Self::Add(_) => "add",
            Self::Remove(_) => "remove",
            Self::Clear => "clear",
        }
    }

    fn path(&self) -> Option<&str> {
        match self {
            Self::Add(path) | Self::Remove(path) => Some(path),
            Self::Clear => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    pub action: &'static str,
    pub path: Option<String>,
    pub saved_changed: bool,
    pub runtime_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconciliation {
    Intended,
    Previous,
    Unconfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Updated {
        access: WorkspaceAccess,
        mutation: Mutation,
    },
    Indeterminate(Reconciliation),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("{0}")]
    Directory(#[from] DirectoryError),
    #[error("HomeNotSet")]
    HomeNotSet,
    #[error("InvalidSettingsFormat")]
    InvalidSettingsFormat,
    #[error("{0}")]
    Settings(SettingsWriteError),
    #[error("{0}")]
    Load(SettingsError),
}

enum Patch {
    Add(String),
    Remove(String),
    Clear,
}

pub fn execute(
    paths: Option<&ProfilePaths>,
    primary: &Path,
    current: &WorkspaceAccess,
    action: &Action,
) -> Result<Outcome, CommandError> {
    let primary_text = primary.to_str().ok_or(DirectoryError::InvalidPath)?;
    let (staged, patch) = match action {
        Action::Add(path) => {
            let identity = canonical_existing_directory(primary_text, path)?;
            let staged = match current.stage_add_saved(primary_text, &identity) {
                Ok(staged) => Some(staged),
                Err(DirectoryError::TooManyDirectories) => None,
                Err(error) => return Err(error.into()),
            };
            (staged, Patch::Add(identity))
        }
        Action::Remove(path) => {
            let staged = current.stage_remove(primary_text, path)?;
            let removed = current
                .removed_path(&staged)
                .ok_or(DirectoryError::UnknownAdditionalDirectory)?
                .to_owned();
            (Some(staged), Patch::Remove(removed))
        }
        Action::Clear => (Some(WorkspaceAccess::default()), Patch::Clear),
    };
    let paths = paths.ok_or(CommandError::HomeNotSet)?;
    let observed = current.saved_sources();
    let saved_changed =
        match save_workspace_entry(paths, primary, |entry| apply(entry, &patch, observed)) {
            Ok(changed) => changed,
            Err(WorkspaceSaveError::Edit(error)) => return Err(error),
            Err(WorkspaceSaveError::Settings(failure))
                if failure.error == SettingsWriteError::CommitIndeterminate =>
            {
                return Ok(Outcome::Indeterminate(reconcile(
                    paths,
                    primary,
                    current,
                    staged.as_ref(),
                )));
            }
            Err(WorkspaceSaveError::Settings(failure)) => {
                return Err(CommandError::Settings(failure.error));
            }
        };
    let committed = load_committed(paths, primary)?;
    let runtime_changed = current.entries() != committed.entries();
    Ok(Outcome::Updated {
        access: committed,
        mutation: Mutation {
            action: action.label(),
            path: action.path().map(str::to_owned),
            saved_changed,
            runtime_changed,
        },
    })
}

fn load_committed(paths: &ProfilePaths, primary: &Path) -> Result<WorkspaceAccess, CommandError> {
    let settings = Settings::load(paths, primary).map_err(CommandError::Load)?;
    if settings.additional_directories_rejected() {
        return Err(CommandError::InvalidSettingsFormat);
    }
    let primary = primary.to_str().ok_or(DirectoryError::InvalidPath)?;
    Ok(WorkspaceAccess::new(
        primary,
        settings.additional_directory_sources(),
    )?)
}

fn reconcile(
    paths: &ProfilePaths,
    primary: &Path,
    current: &WorkspaceAccess,
    intended: Option<&WorkspaceAccess>,
) -> Reconciliation {
    let (Some(intended), Ok(durable)) = (intended, load_committed(paths, primary)) else {
        return Reconciliation::Unconfirmed;
    };
    let durable = durable.saved_directories();
    if durable == intended.saved_directories() {
        Reconciliation::Intended
    } else if durable == current.saved_directories() {
        Reconciliation::Previous
    } else {
        Reconciliation::Unconfirmed
    }
}

fn apply(
    entry: &mut Value,
    change: &Patch,
    observed: &[SavedSource],
) -> Result<bool, CommandError> {
    let mut workspace = match entry {
        Value::Null => Map::new(),
        Value::Object(workspace) => workspace.clone(),
        _ => return Err(CommandError::InvalidSettingsFormat),
    };
    let changed = match change {
        Patch::Clear => workspace.shift_remove(DIRECTORIES_KEY).is_some(),
        Patch::Add(path) => add(&mut workspace, path, observed)?,
        Patch::Remove(path) => remove(&mut workspace, path, observed)?,
    };
    *entry = Value::Object(workspace);
    Ok(changed)
}

fn stored(workspace: &Map<String, Value>) -> Result<Vec<String>, CommandError> {
    match workspace.get(DIRECTORIES_KEY) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or(CommandError::InvalidSettingsFormat)
            })
            .collect(),
        Some(_) => Err(CommandError::InvalidSettingsFormat),
    }
}

fn observed_identity<'a>(observed: &'a [SavedSource], source: &str) -> Option<&'a str> {
    observed
        .iter()
        .find(|saved| saved.source == source)
        .map(|saved| saved.identity.as_str())
}

fn add(
    workspace: &mut Map<String, Value>,
    path: &str,
    observed: &[SavedSource],
) -> Result<bool, CommandError> {
    let existing = stored(workspace)?;
    let unseen: Vec<&String> = existing
        .iter()
        .filter(|source| observed_identity(observed, source).is_none())
        .collect();
    let mut saved_contains_path = unseen.iter().any(|source| *source == path);
    let mut identities: Vec<&str> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for source in &existing {
        let Some(identity) = observed_identity(observed, source) else {
            kept.push(source.clone());
            continue;
        };
        saved_contains_path |= identity == path;
        if unseen.iter().any(|unseen| *unseen == identity) || identities.contains(&identity) {
            continue;
        }
        identities.push(identity);
        kept.push(identity.to_owned());
    }
    if !saved_contains_path && !identities.contains(&path) {
        identities.push(path);
    }
    if identities.len() + unseen.len() > MAX_ADDITIONAL_DIRECTORIES {
        return Err(DirectoryError::TooManyDirectories.into());
    }
    if !saved_contains_path {
        kept.push(path.to_owned());
    }
    Ok(store(workspace, &existing, kept))
}

fn remove(
    workspace: &mut Map<String, Value>,
    path: &str,
    observed: &[SavedSource],
) -> Result<bool, CommandError> {
    if !observed.iter().any(|saved| saved.identity == path)
        || !workspace.contains_key(DIRECTORIES_KEY)
    {
        return Ok(false);
    }
    let existing = stored(workspace)?;
    let unseen: Vec<&String> = existing
        .iter()
        .filter(|source| observed_identity(observed, source).is_none())
        .collect();
    let mut survivors: Vec<&str> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for source in &existing {
        match observed_identity(observed, source) {
            None if source == path => {}
            None => kept.push(source.clone()),
            Some(identity)
                if identity == path
                    || unseen.iter().any(|unseen| *unseen == identity)
                    || survivors.contains(&identity) => {}
            Some(identity) => {
                survivors.push(identity);
                kept.push(identity.to_owned());
            }
        }
    }
    Ok(store(workspace, &existing, kept))
}

fn store(workspace: &mut Map<String, Value>, existing: &[String], kept: Vec<String>) -> bool {
    if kept == existing {
        return false;
    }
    if kept.is_empty() {
        workspace.shift_remove(DIRECTORIES_KEY);
    } else {
        workspace.insert(
            DIRECTORIES_KEY.to_owned(),
            Value::Array(kept.into_iter().map(Value::String).collect()),
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn source(source: &str, identity: &str) -> SavedSource {
        SavedSource {
            source: source.to_owned(),
            identity: identity.to_owned(),
        }
    }

    fn patched(entry: Value, patch: &Patch, observed: &[SavedSource]) -> (Value, bool) {
        let mut entry = entry;
        let changed = apply(&mut entry, patch, observed).unwrap();
        (entry, changed)
    }

    #[test]
    fn adding_canonicalizes_observed_spellings_and_keeps_unseen_entries() {
        let observed = [source("/a/./x", "/a/x")];
        let (entry, changed) = patched(
            json!({"additional_directories": ["/a/./x", "/new/elsewhere"], "model": "m"}),
            &Patch::Add("/b".to_owned()),
            &observed,
        );
        assert!(changed);
        assert_eq!(
            entry,
            json!({"additional_directories": ["/a/x", "/new/elsewhere", "/b"], "model": "m"})
        );
        let (entry, changed) = patched(
            json!({"additional_directories": ["/a/x"]}),
            &Patch::Add("/a/x".to_owned()),
            &[source("/a/x", "/a/x")],
        );
        assert!(!changed);
        assert_eq!(entry, json!({"additional_directories": ["/a/x"]}));
        let (entry, changed) = patched(Value::Null, &Patch::Add("/b".to_owned()), &[]);
        assert!(changed);
        assert_eq!(entry, json!({"additional_directories": ["/b"]}));
    }

    #[test]
    fn adding_counts_unseen_entries_against_the_limit() {
        let unseen: Vec<String> = (0..MAX_ADDITIONAL_DIRECTORIES)
            .map(|index| format!("/d{index}"))
            .collect();
        let mut entry = json!({ "additional_directories": unseen });
        assert_eq!(
            apply(&mut entry, &Patch::Add("/b".to_owned()), &[]),
            Err(CommandError::Directory(DirectoryError::TooManyDirectories))
        );
    }

    #[test]
    fn removing_drops_every_spelling_of_the_identity_and_the_empty_key() {
        let observed = [source("/a/./x", "/a/x"), source("/b", "/b")];
        let (entry, changed) = patched(
            json!({"additional_directories": ["/a/./x", "/b"]}),
            &Patch::Remove("/a/x".to_owned()),
            &observed,
        );
        assert!(changed);
        assert_eq!(entry, json!({"additional_directories": ["/b"]}));
        let (entry, changed) = patched(
            json!({"additional_directories": ["/b"], "model": "m"}),
            &Patch::Remove("/b".to_owned()),
            &observed,
        );
        assert!(changed);
        assert_eq!(entry, json!({"model": "m"}));
        let (_, changed) = patched(
            json!({"additional_directories": ["/b"]}),
            &Patch::Remove("/c".to_owned()),
            &observed,
        );
        assert!(!changed);
    }

    #[test]
    fn malformed_entries_are_refused() {
        for entry in [
            json!([1]),
            json!({"additional_directories": "x"}),
            json!({"additional_directories": [1]}),
        ] {
            let mut entry = entry;
            assert_eq!(
                apply(&mut entry, &Patch::Add("/b".to_owned()), &[]),
                Err(CommandError::InvalidSettingsFormat)
            );
        }
    }
}
