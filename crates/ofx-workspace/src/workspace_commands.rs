use std::ffi::OsString;
use std::path::Path;

use ofx_config::{
    ProfilePaths, Settings, SettingsError, SettingsWriteError, WorkspaceSaveError,
    save_workspace_entry,
};
use serde_json::{Map, Value};

use crate::workspace_access::{MAX_ADDITIONAL_DIRECTORIES, WorkspaceAccess, WorkspaceAccessError};

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
    pub launch_flag_can_restore: bool,
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
    Access(#[from] WorkspaceAccessError),
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

struct Observed<'a> {
    source: &'a str,
    identity: &'a str,
}

struct DurablePatch<'a> {
    change: Patch,
    observed: Vec<Observed<'a>>,
    command_line: Vec<&'a str>,
}

pub fn execute(
    paths: Option<&ProfilePaths>,
    current: &WorkspaceAccess,
    action: &Action,
) -> Result<Outcome, CommandError> {
    let (staged, change) = stage(current, action)?;
    let runtime = staged.as_ref().unwrap_or(current);
    let command_line = match action {
        Action::Add(_) => runtime
            .command_line_directories()
            .map(text)
            .collect::<Result<_, _>>()?,
        Action::Remove(_) | Action::Clear => Vec::new(),
    };
    let observed = current
        .saved_sources()
        .iter()
        .map(|saved| {
            Ok(Observed {
                source: &saved.source,
                identity: text(&saved.identity)?,
            })
        })
        .collect::<Result<_, CommandError>>()?;
    let patch = DurablePatch {
        change,
        observed,
        command_line,
    };
    let paths = paths.ok_or(CommandError::HomeNotSet)?;
    let saved_changed =
        match save_workspace_entry(paths, current.primary(), |entry| apply(entry, &patch)) {
            Ok(changed) => changed,
            Err(WorkspaceSaveError::Edit(error)) => return Err(error),
            Err(WorkspaceSaveError::Settings(failure))
                if failure.error == SettingsWriteError::CommitIndeterminate =>
            {
                return reconcile(paths, current, staged.as_ref()).map(Outcome::Indeterminate);
            }
            Err(WorkspaceSaveError::Settings(failure)) => {
                return Err(CommandError::Settings(failure.error));
            }
        };
    let committed = load_committed(paths, runtime)?;
    let mutation = Mutation {
        action: action.label(),
        path: action.path().map(str::to_owned),
        saved_changed,
        runtime_changed: current.entries() != committed.entries(),
        launch_flag_can_restore: current.command_line_source_removed(&committed),
    };
    Ok(Outcome::Updated {
        access: committed,
        mutation,
    })
}

fn stage(
    current: &WorkspaceAccess,
    action: &Action,
) -> Result<(Option<WorkspaceAccess>, Patch), CommandError> {
    Ok(match action {
        Action::Add(path) => {
            let identity = text(&current.add_directory_identity(path)?)?.to_owned();
            let staged = match current.stage_add_saved(&identity) {
                Ok(staged) => Some(staged),
                Err(WorkspaceAccessError::TooManyDirectories) => None,
                Err(error) => return Err(error.into()),
            };
            (staged, Patch::Add(identity))
        }
        Action::Remove(path) => {
            let staged = current.stage_remove(path)?;
            let removed = removed_path(current, &staged)
                .ok_or(WorkspaceAccessError::UnknownAdditionalDirectory)?;
            let removed = text(removed)?.to_owned();
            (Some(staged), Patch::Remove(removed))
        }
        Action::Clear => (Some(current.stage_clear()), Patch::Clear),
    })
}

fn text(path: &Path) -> Result<&str, CommandError> {
    path.to_str()
        .ok_or(CommandError::Access(WorkspaceAccessError::InvalidPath))
}

fn removed_path<'a>(current: &'a WorkspaceAccess, staged: &WorkspaceAccess) -> Option<&'a Path> {
    current
        .entries()
        .iter()
        .find(|entry| !staged.entries().iter().any(|kept| kept.path == entry.path))
        .map(|entry| entry.path.as_path())
}

fn load_committed(
    paths: &ProfilePaths,
    runtime: &WorkspaceAccess,
) -> Result<WorkspaceAccess, CommandError> {
    let settings = Settings::load(paths, runtime.primary()).map_err(CommandError::Load)?;
    if settings.additional_directories_rejected() {
        return Err(CommandError::InvalidSettingsFormat);
    }
    let command_line: Vec<OsString> = runtime
        .command_line_directories()
        .map(|path| path.as_os_str().to_owned())
        .collect();
    Ok(
        WorkspaceAccess::new(runtime.primary(), settings.additional_directories())?
            .apply_launch(&command_line, runtime.saved_suppressed())?,
    )
}

fn reconcile(
    paths: &ProfilePaths,
    current: &WorkspaceAccess,
    intended: Option<&WorkspaceAccess>,
) -> Result<Reconciliation, CommandError> {
    let Some(intended) = intended else {
        return Ok(Reconciliation::Unconfirmed);
    };
    let settings = Settings::load(paths, current.primary()).map_err(CommandError::Load)?;
    if settings.additional_directories_rejected() {
        return Ok(Reconciliation::Unconfirmed);
    }
    let Ok(durable) = WorkspaceAccess::new(current.primary(), settings.additional_directories())
    else {
        return Ok(Reconciliation::Unconfirmed);
    };
    Ok(classify(current, intended, &durable))
}

fn classify(
    current: &WorkspaceAccess,
    intended: &WorkspaceAccess,
    durable: &WorkspaceAccess,
) -> Reconciliation {
    if durable.saved_directories().eq(intended.saved_directories()) {
        Reconciliation::Intended
    } else if durable.saved_directories().eq(current.saved_directories()) {
        Reconciliation::Previous
    } else {
        Reconciliation::Unconfirmed
    }
}

fn apply(entry: &mut Value, durable: &DurablePatch<'_>) -> Result<bool, CommandError> {
    let mut workspace = match entry {
        Value::Null => Map::new(),
        Value::Object(workspace) => workspace.clone(),
        _ => return Err(CommandError::InvalidSettingsFormat),
    };
    let changed = match &durable.change {
        Patch::Clear => workspace.shift_remove(DIRECTORIES_KEY).is_some(),
        Patch::Add(path) => add(&mut workspace, path, durable)?,
        Patch::Remove(path) => remove(&mut workspace, path, &durable.observed)?,
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

fn observed_identity<'a>(observed: &[Observed<'a>], source: &str) -> Option<&'a str> {
    observed
        .iter()
        .find(|saved| saved.source == source)
        .map(|saved| saved.identity)
}

fn add(
    workspace: &mut Map<String, Value>,
    path: &str,
    durable: &DurablePatch<'_>,
) -> Result<bool, CommandError> {
    let existing = stored(workspace)?;
    let unseen: Vec<&str> = existing
        .iter()
        .map(String::as_str)
        .filter(|source| observed_identity(&durable.observed, source).is_none())
        .collect();
    let mut saved_contains_path = unseen.contains(&path);
    let mut identities: Vec<&str> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for source in &existing {
        let Some(identity) = observed_identity(&durable.observed, source) else {
            kept.push(source.clone());
            continue;
        };
        saved_contains_path |= identity == path;
        if unseen.contains(&identity) || identities.contains(&identity) {
            continue;
        }
        identities.push(identity);
        kept.push(identity.to_owned());
    }
    for command_line in &durable.command_line {
        if !unseen.contains(command_line) && !identities.contains(command_line) {
            identities.push(command_line);
        }
    }
    if !saved_contains_path && !identities.contains(&path) {
        identities.push(path);
    }
    if identities.len() + unseen.len() > MAX_ADDITIONAL_DIRECTORIES {
        return Err(WorkspaceAccessError::TooManyDirectories.into());
    }
    if !saved_contains_path {
        kept.push(path.to_owned());
    }
    Ok(store(workspace, &existing, kept))
}

fn remove(
    workspace: &mut Map<String, Value>,
    path: &str,
    observed: &[Observed<'_>],
) -> Result<bool, CommandError> {
    if !observed.iter().any(|saved| saved.identity == path)
        || !workspace.contains_key(DIRECTORIES_KEY)
    {
        return Ok(false);
    }
    let existing = stored(workspace)?;
    let unseen: Vec<&str> = existing
        .iter()
        .map(String::as_str)
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
                    || unseen.contains(&identity)
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
mod tests;
