use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ofx_contract::{PermissionAction, PermissionMode, StatuslineItem, parse_strict_json_value};
use serde_json::{Map, Value};

use crate::config_runtime::{
    BYTE_ORDER_MARK, LayerScope, MAX_SETTINGS_BYTES, SETTINGS_FILE, is_valid_profile_layer,
};
use crate::configured_provider::is_valid_model_id;
use crate::io::{AdvisoryLock, DurableError, PrivateDir};
use crate::model_provider::ProviderId;
use crate::paths::ProfilePaths;

pub(crate) const MAX_PROVIDER_ORDER_ENTRIES: usize = 8;
const MAX_PROVIDER_SLUG_BYTES: usize = 64;
const LOCK_FILE: &str = "settings.lock";
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(10);
const COMMIT_ATTEMPTS: usize = 3;
const BACKUPS_DIRECTORY: &str = "backups";
const BACKUP_KEEP_COUNT: usize = 5;
const CORRUPT_KEEP_COUNT: usize = 3;
const RETIRED_SETTINGS: [&str; 2] = ["input_appearance", "maxxing_mode"];
const FAST_MODE: &str = "fast_mode";
const EFFORT: &str = "effort";
const FAST_MODE_MODEL_BOUND: &str = "fast_mode_model_bound";
const LEGACY_CODEX_MODEL: &str = "codex_model";
const PERMISSION_MODE: &str = "permission_mode";
const YOLO_ACKNOWLEDGED: &str = "yolo_acknowledged";
const STARTUP_SCROLLBACK: &str = "startup_scrollback";
const PERMISSION_MODE_MIGRATION: Migration = Migration {
    container: None,
    field: PERMISSION_MODE,
    binding: None,
    snapshot: "settings.json.preference-migration.permission_mode.json",
};
const STARTUP_SCROLLBACK_MIGRATION: Migration = Migration {
    container: None,
    field: STARTUP_SCROLLBACK,
    binding: None,
    snapshot: "settings.json.preference-migration.startup_scrollback.json",
};
const SESSION_TITLES: &str = "session_titles";
const SESSION_TITLES_MIGRATION: Migration = Migration {
    container: None,
    field: SESSION_TITLES,
    binding: None,
    snapshot: "settings.json.preference-migration.session_titles.json",
};
const PROMPT_HISTORY: &str = "prompt_history";
const PROMPT_HISTORY_MIGRATION: Migration = Migration {
    container: Some(PROMPT_HISTORY),
    field: "enabled",
    binding: None,
    snapshot: "settings.json.preference-migration.prompt_history_enabled.json",
};
const STATUSLINE: &str = "statusLine";
const STATUSLINE_CONTEXT_MIGRATION: Migration = Migration {
    container: Some(STATUSLINE),
    field: "context",
    binding: None,
    snapshot: "settings.json.preference-migration.statusline_context.json",
};
const STATUSLINE_SESSION_MIGRATION: Migration = Migration {
    container: Some(STATUSLINE),
    field: "session",
    binding: None,
    snapshot: "settings.json.preference-migration.statusline_session.json",
};
const FAST_MODE_MIGRATION: Migration = Migration {
    container: None,
    field: FAST_MODE,
    binding: Some(FAST_MODE_MODEL_BOUND),
    snapshot: "settings.json.preference-migration.fast_mode.json",
};
const EFFORT_MIGRATION: Migration = Migration {
    container: None,
    field: EFFORT,
    binding: None,
    snapshot: "settings.json.preference-migration.effort.json",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SettingsWriteError {
    #[error("DurablePathUnsafe")]
    DurablePathUnsafe,
    #[error("PrivateStatePermissionsUnsupported")]
    PermissionsUnsupported,
    #[error("AccessDenied")]
    AccessDenied,
    #[error("SettingsLockBusy")]
    LockBusy,
    #[error("SettingsLockUnsupported")]
    LockUnsupported,
    #[error("SettingsPrimaryTooLarge")]
    PrimaryTooLarge,
    #[error("InvalidSettingsFormat")]
    InvalidFormat,
    #[error("InvalidDurableField")]
    InvalidField,
    #[error("SettingsTooLarge")]
    TooLarge,
    #[error("SettingsNumberNotPreserved")]
    NumberNotPreserved,
    #[error("SettingsBackupFailed")]
    BackupFailed,
    #[error("SettingsMigrationSnapshotFailed")]
    MigrationSnapshotFailed,
    #[error("SettingsConcurrentModification")]
    ConcurrentModification,
    #[error("SettingsCommitFailed")]
    CommitFailed,
    #[error("SettingsCommitIndeterminate")]
    CommitIndeterminate,
    #[error("SettingsStoreUnavailable")]
    Unavailable,
}

impl From<DurableError> for SettingsWriteError {
    fn from(error: DurableError) -> Self {
        match error {
            DurableError::PathUnsafe => Self::DurablePathUnsafe,
            DurableError::PermissionsUnsupported => Self::PermissionsUnsupported,
            DurableError::AccessDenied => Self::AccessDenied,
            DurableError::LockUnsupported => Self::LockUnsupported,
            DurableError::TooLarge => Self::PrimaryTooLarge,
            DurableError::PreRenameFailed => Self::CommitFailed,
            DurableError::PostRenameFailed => Self::CommitIndeterminate,
            DurableError::InsecureFile | DurableError::Failed => Self::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyCleanup {
    pub fields_removed: usize,
    pub workspaces_changed: usize,
    pub recovery_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{error}")]
pub struct SettingsWriteFailure {
    pub error: SettingsWriteError,
    pub cleanup: LegacyCleanup,
}

impl From<SettingsWriteError> for SettingsWriteFailure {
    fn from(error: SettingsWriteError) -> Self {
        Self {
            error,
            cleanup: LegacyCleanup::default(),
        }
    }
}

impl From<DurableError> for SettingsWriteFailure {
    fn from(error: DurableError) -> Self {
        SettingsWriteError::from(error).into()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowlistResetScope {
    All,
    Commands,
    Tools,
    Urls,
    WebFetchDomains,
}

impl AllowlistResetScope {
    fn matches_category(self, category: &str) -> bool {
        let canonical = category.trim_matches([' ', '\t', '\r', '\n']);
        let is_url = matches!(canonical, "url" | "open_url" | "browser_navigate");
        let is_fetch = canonical == "web_fetch";
        match self {
            Self::All => true,
            Self::Commands => canonical == "bash",
            Self::Urls => is_url,
            Self::WebFetchDomains => is_fetch,
            Self::Tools => canonical != "bash" && !is_url && !is_fetch && canonical != "*",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPatch<'a> {
    Add {
        category: &'a str,
        pattern: &'a str,
        action: PermissionAction,
    },
    Remove {
        category: &'a str,
        pattern: &'a str,
    },
    Reset(AllowlistResetScope),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    Unchanged,
    Committed {
        permission_rules_removed: usize,
        cleanup: LegacyCleanup,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSaveError<E> {
    Edit(E),
    Settings(SettingsWriteFailure),
}

type WorkspaceEdit<'a> = &'a dyn Fn(&mut Value) -> Result<bool, SettingsWriteError>;

#[derive(Clone, Copy)]
enum Patch<'a> {
    CodexModel(&'a str),
    ModelPreference {
        provider: &'a ProviderId,
        model: &'a str,
        effort: Option<&'a str>,
        fast_mode: bool,
    },
    PermissionMode(PermissionMode),
    YoloAcknowledged,
    StartupScrollback(bool),
    SessionTitles(bool),
    PromptHistoryEnabled(bool),
    StatuslineItem {
        item: StatuslineItem,
        enabled: bool,
    },
    Permission {
        workspace: Option<&'a str>,
        patch: PermissionPatch<'a>,
    },
    WorkspaceEntry {
        key: &'a str,
        edit: WorkspaceEdit<'a>,
    },
}

#[derive(Debug, Default)]
struct Application {
    changed: bool,
    fields_removed: usize,
    workspaces_changed: usize,
    permission_rules_removed: usize,
    migration_snapshots: Vec<&'static str>,
}

struct Migration {
    container: Option<&'static str>,
    field: &'static str,
    binding: Option<&'static str>,
    snapshot: &'static str,
}

pub fn save_codex_model(paths: &ProfilePaths, model: &str) -> Result<(), SettingsWriteError> {
    commit(paths, Patch::CodexModel(model), &mut || {})
        .map(drop)
        .map_err(|failure| failure.error)
}

pub fn save_model_preference(
    paths: &ProfilePaths,
    provider: &ProviderId,
    model: &str,
    effort: Option<&str>,
    fast_mode: bool,
) -> Result<(), SettingsWriteFailure> {
    commit(
        paths,
        Patch::ModelPreference {
            provider,
            model,
            effort,
            fast_mode,
        },
        &mut || {},
    )
    .map(drop)
}

pub fn save_permission_mode(
    paths: &ProfilePaths,
    mode: PermissionMode,
) -> Result<(), SettingsWriteFailure> {
    commit(paths, Patch::PermissionMode(mode), &mut || {}).map(drop)
}

pub fn save_yolo_acknowledged(paths: &ProfilePaths) -> Result<(), SettingsWriteFailure> {
    commit(paths, Patch::YoloAcknowledged, &mut || {}).map(drop)
}

pub fn save_startup_scrollback(
    paths: &ProfilePaths,
    enabled: bool,
) -> Result<CommitOutcome, SettingsWriteFailure> {
    commit(paths, Patch::StartupScrollback(enabled), &mut || {})
}

pub fn save_session_titles(
    paths: &ProfilePaths,
    enabled: bool,
) -> Result<CommitOutcome, SettingsWriteFailure> {
    commit(paths, Patch::SessionTitles(enabled), &mut || {})
}

pub fn save_prompt_history_enabled(
    paths: &ProfilePaths,
    enabled: bool,
) -> Result<CommitOutcome, SettingsWriteFailure> {
    commit(paths, Patch::PromptHistoryEnabled(enabled), &mut || {})
}

pub fn save_statusline_item(
    paths: &ProfilePaths,
    item: StatuslineItem,
    enabled: bool,
) -> Result<CommitOutcome, SettingsWriteFailure> {
    commit(paths, Patch::StatuslineItem { item, enabled }, &mut || {})
}

pub fn save_permission_patch(
    paths: &ProfilePaths,
    workspace_root: Option<&Path>,
    patch: PermissionPatch<'_>,
) -> Result<CommitOutcome, SettingsWriteFailure> {
    if workspace_root.is_some_and(|root| !root.is_absolute()) {
        return Err(SettingsWriteError::InvalidField.into());
    }
    let workspace = workspace_root.map(Path::to_string_lossy);
    commit(
        paths,
        Patch::Permission {
            workspace: workspace.as_deref(),
            patch,
        },
        &mut || {},
    )
}

pub fn save_workspace_entry<E>(
    paths: &ProfilePaths,
    workspace_root: &Path,
    edit: impl Fn(&mut Value) -> Result<bool, E>,
) -> Result<bool, WorkspaceSaveError<E>> {
    let key = workspace_root.to_string_lossy();
    let refused = RefCell::new(None);
    let changed = Cell::new(false);
    let apply_edit = |entry: &mut Value| match edit(entry) {
        Ok(edited) => {
            changed.set(edited);
            Ok(edited)
        }
        Err(error) => {
            *refused.borrow_mut() = Some(error);
            Err(SettingsWriteError::InvalidField)
        }
    };
    let patch = Patch::WorkspaceEntry {
        key: &key,
        edit: &apply_edit,
    };
    let committed = commit(paths, patch, &mut || {});
    if let Some(error) = refused.into_inner() {
        return Err(WorkspaceSaveError::Edit(error));
    }
    committed.map_err(WorkspaceSaveError::Settings)?;
    Ok(changed.get())
}

fn commit(
    paths: &ProfilePaths,
    patch: Patch<'_>,
    before_commit: &mut dyn FnMut(),
) -> Result<CommitOutcome, SettingsWriteFailure> {
    let directory = PrivateDir::open_or_create(&paths.config)?;
    let _lock = lock(&directory)?;
    for _ in 0..COMMIT_ATTEMPTS {
        let existing = directory.read_owned(SETTINGS_FILE, MAX_SETTINGS_BYTES)?;
        let mut root = match &existing {
            Some(bytes) => parse_root(bytes).ok_or_else(|| {
                let _ = create_sequenced_copy(&directory, SettingsCopy::Corrupt, bytes);
                SettingsWriteError::InvalidFormat
            })?,
            None => Map::new(),
        };
        let application = apply(&mut root, patch)?;
        if !application.changed {
            return Ok(CommitOutcome::Unchanged);
        }
        if existing.as_deref().is_some_and(has_unpreserved_number) {
            return Err(SettingsWriteError::NumberNotPreserved.into());
        }
        let candidate = serialize(&root);
        if candidate.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsWriteError::TooLarge.into());
        }
        validate_candidate(&candidate, patch)?;
        let recovery_paths = application
            .migration_snapshots
            .iter()
            .map(|snapshot| {
                write_migration_snapshot(&directory, paths, existing.as_deref(), snapshot)
            })
            .collect::<Result<Vec<_>, _>>()?;
        before_commit();
        if directory.read_owned(SETTINGS_FILE, MAX_SETTINGS_BYTES)? != existing {
            continue;
        }
        if let Some(bytes) = &existing {
            create_sequenced_copy(&directory, SettingsCopy::Backup, bytes)
                .map_err(|_| SettingsWriteError::BackupFailed)?;
        }
        if directory.read_owned(SETTINGS_FILE, MAX_SETTINGS_BYTES)? != existing {
            continue;
        }
        return match directory.replace(SETTINGS_FILE, candidate.as_bytes()) {
            Ok(()) => Ok(CommitOutcome::Committed {
                permission_rules_removed: application.permission_rules_removed,
                cleanup: LegacyCleanup {
                    fields_removed: application.fields_removed,
                    workspaces_changed: application.workspaces_changed,
                    recovery_paths,
                },
            }),
            Err(DurableError::PostRenameFailed) => Err(SettingsWriteFailure {
                error: SettingsWriteError::CommitIndeterminate,
                cleanup: LegacyCleanup {
                    fields_removed: application.fields_removed,
                    workspaces_changed: application.workspaces_changed,
                    recovery_paths,
                },
            }),
            Err(error) => Err(error.into()),
        };
    }
    Err(SettingsWriteError::ConcurrentModification.into())
}

fn write_migration_snapshot(
    directory: &PrivateDir,
    paths: &ProfilePaths,
    existing: Option<&[u8]>,
    snapshot: &str,
) -> Result<PathBuf, SettingsWriteError> {
    let bytes = existing.ok_or(SettingsWriteError::InvalidFormat)?;
    directory
        .open_or_create_child(BACKUPS_DIRECTORY)
        .and_then(|backups| backups.replace(snapshot, bytes))
        .map_err(|_| SettingsWriteError::MigrationSnapshotFailed)?;
    Ok(paths.config.join(BACKUPS_DIRECTORY).join(snapshot))
}

fn lock(directory: &PrivateDir) -> Result<AdvisoryLock, SettingsWriteError> {
    let started = Instant::now();
    loop {
        if let Some(lock) = directory.try_lock(LOCK_FILE)? {
            return Ok(lock);
        }
        if started.elapsed() >= LOCK_WAIT {
            return Err(SettingsWriteError::LockBusy);
        }
        thread::sleep(LOCK_RETRY);
    }
}

fn parse_root(bytes: &[u8]) -> Option<Map<String, Value>> {
    let bytes = bytes.strip_prefix(BYTE_ORDER_MARK).unwrap_or(bytes);
    match parse_strict_json_value(bytes) {
        Ok(Value::Object(root)) => Some(root),
        _ => None,
    }
}

fn has_unpreserved_number(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b'"' {
                    index += if bytes[index] == b'\\' { 2 } else { 1 };
                }
                index += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = index;
                while index < bytes.len()
                    && matches!(bytes[index], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    index += 1;
                }
                let token = std::str::from_utf8(&bytes[start..index]).unwrap_or_default();
                if !rewrites_to_the_same_value(token) {
                    return true;
                }
            }
            _ => index += 1,
        }
    }
    false
}

fn rewrites_to_the_same_value(token: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(token) else {
        return false;
    };
    let mut rewritten = String::new();
    write_value(&mut rewritten, &value);
    match (decimal_value(token), decimal_value(&rewritten)) {
        (Some(original), Some(rewritten)) => original == rewritten,
        _ => false,
    }
}

fn decimal_value(token: &str) -> Option<(bool, String, i64)> {
    let (negative, unsigned) = match token.strip_prefix('-') {
        Some(unsigned) => (true, unsigned),
        None => (false, token),
    };
    let (mantissa, exponent) = unsigned.split_once(['e', 'E']).unwrap_or((unsigned, "0"));
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let significant = digits.trim_end_matches('0');
    if significant.trim_start_matches('0').is_empty() {
        return Some((false, String::new(), 0));
    }
    let scale = i64::try_from(digits.len() - significant.len())
        .ok()?
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(exponent.parse::<i64>().ok()?)?;
    Some((
        negative,
        significant.trim_start_matches('0').to_owned(),
        scale,
    ))
}

fn apply(
    root: &mut Map<String, Value>,
    patch: Patch<'_>,
) -> Result<Application, SettingsWriteError> {
    if root
        .get("workspaces")
        .is_some_and(|workspaces| !workspaces.is_object())
    {
        return Err(SettingsWriteError::InvalidFormat);
    }
    let mut application = Application {
        changed: remove_retired_settings(root),
        ..Application::default()
    };
    match patch {
        Patch::CodexModel(model) => {
            application.changed |= put_model(root, &ProviderId::Codex, model)?;
            application.changed |= put_string(root, "provider", ProviderId::Codex.label());
            if root.shift_remove(FAST_MODE_MODEL_BOUND).is_some() {
                application.changed = true;
            }
            clear_workspace_fast_mode_bindings(root);
        }
        Patch::ModelPreference {
            provider,
            model,
            effort,
            fast_mode,
        } => {
            application.changed |= put_model(root, provider, model)?;
            application.changed |= put_string(root, "provider", provider.label());
            if let Some(effort) = effort {
                application.changed |= put_string(root, EFFORT, effort);
                migrate_workspace_preference(root, &EFFORT_MIGRATION, &mut application);
            }
            application.changed |= put_bool(root, FAST_MODE, fast_mode);
            application.changed |= put_bool(root, FAST_MODE_MODEL_BOUND, true);
            migrate_workspace_preference(root, &FAST_MODE_MIGRATION, &mut application);
        }
        Patch::PermissionMode(mode) => {
            application.changed |= put_string(root, PERMISSION_MODE, mode.label());
            migrate_workspace_preference(root, &PERMISSION_MODE_MIGRATION, &mut application);
        }
        Patch::YoloAcknowledged => application.changed |= put_bool(root, YOLO_ACKNOWLEDGED, true),
        Patch::StatuslineItem { item, enabled } => {
            let Value::Object(statusline) = root
                .entry(STATUSLINE)
                .or_insert_with(|| Value::Object(Map::new()))
            else {
                return Err(SettingsWriteError::InvalidFormat);
            };
            application.changed |= put_bool(statusline, item.label(), enabled);
            let migration = match item {
                StatuslineItem::Context => Some(&STATUSLINE_CONTEXT_MIGRATION),
                StatuslineItem::Session => Some(&STATUSLINE_SESSION_MIGRATION),
                StatuslineItem::Workspace => None,
            };
            if let Some(migration) = migration {
                migrate_workspace_preference(root, migration, &mut application);
            }
        }
        Patch::StartupScrollback(enabled) => {
            application.changed |= put_bool(root, STARTUP_SCROLLBACK, enabled);
            migrate_workspace_preference(root, &STARTUP_SCROLLBACK_MIGRATION, &mut application);
        }
        Patch::SessionTitles(enabled) => {
            application.changed |= put_bool(root, SESSION_TITLES, enabled);
            migrate_workspace_preference(root, &SESSION_TITLES_MIGRATION, &mut application);
        }
        Patch::PromptHistoryEnabled(enabled) => {
            let Value::Object(history) = root
                .entry(PROMPT_HISTORY)
                .or_insert_with(|| Value::Object(Map::new()))
            else {
                return Err(SettingsWriteError::InvalidFormat);
            };
            application.changed |= put_bool(history, "enabled", enabled);
            migrate_workspace_preference(root, &PROMPT_HISTORY_MIGRATION, &mut application);
        }
        Patch::Permission { workspace, patch } => {
            let target = match workspace {
                Some(workspace_root) => workspace_object(root, workspace_root)?,
                None => root,
            };
            let (changed, removed) = apply_permission_patch(target, patch)?;
            application.changed |= changed;
            application.permission_rules_removed = removed;
        }
        Patch::WorkspaceEntry { key, edit } => {
            application.changed |= edit_workspace(root, key, edit)?;
        }
    }
    Ok(application)
}

fn edit_workspace(
    root: &mut Map<String, Value>,
    key: &str,
    edit: WorkspaceEdit<'_>,
) -> Result<bool, SettingsWriteError> {
    let mut entry = root
        .get("workspaces")
        .and_then(|workspaces| workspaces.get(key))
        .cloned()
        .unwrap_or(Value::Null);
    if !edit(&mut entry)? {
        return Ok(false);
    }
    let Value::Object(workspaces) = root
        .entry("workspaces")
        .or_insert_with(|| Value::Object(Map::new()))
    else {
        return Err(SettingsWriteError::InvalidFormat);
    };
    match entry {
        Value::Object(entry) if !entry.is_empty() => {
            workspaces.insert(key.to_owned(), Value::Object(entry));
        }
        _ => {
            workspaces.shift_remove(key);
            if workspaces.is_empty() {
                root.shift_remove("workspaces");
            }
        }
    }
    Ok(true)
}

fn workspace_object<'r>(
    root: &'r mut Map<String, Value>,
    workspace_root: &str,
) -> Result<&'r mut Map<String, Value>, SettingsWriteError> {
    let Value::Object(workspaces) = root
        .entry("workspaces")
        .or_insert_with(|| Value::Object(Map::new()))
    else {
        return Err(SettingsWriteError::InvalidFormat);
    };
    match workspaces
        .entry(workspace_root)
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(workspace) => Ok(workspace),
        _ => Err(SettingsWriteError::InvalidFormat),
    }
}

fn apply_permission_patch(
    target: &mut Map<String, Value>,
    patch: PermissionPatch<'_>,
) -> Result<(bool, usize), SettingsWriteError> {
    match patch {
        PermissionPatch::Add {
            category,
            pattern,
            action,
        } => {
            let permission =
                permission_object(target, true)?.ok_or(SettingsWriteError::InvalidFormat)?;
            let rules = permission
                .entry(category)
                .or_insert_with(|| Value::Object(Map::new()));
            if !rules.is_object() {
                let mut replacement = Map::new();
                replacement.insert("*".to_owned(), rules.take());
                *rules = Value::Object(replacement);
            }
            let Value::Object(rules) = rules else {
                return Err(SettingsWriteError::InvalidFormat);
            };
            Ok((put_string(rules, pattern, action.label()), 0))
        }
        PermissionPatch::Remove { category, pattern } => {
            let Some(permission) = permission_object(target, false)? else {
                return Ok((false, 0));
            };
            let Some(category_key) = canonical_permission_key(permission, category) else {
                return Ok((false, 0));
            };
            let Some(Value::Object(rules)) = permission.get_mut(&category_key) else {
                return Ok((false, 0));
            };
            let Some(pattern_key) = canonical_permission_key(rules, pattern) else {
                return Ok((false, 0));
            };
            rules.shift_remove(&pattern_key);
            if rules.is_empty() {
                permission.shift_remove(&category_key);
            }
            Ok((true, 1))
        }
        PermissionPatch::Reset(scope) => {
            let Some(permission) = permission_object(target, false)? else {
                return Ok((false, 0));
            };
            let mut removed = 0;
            while remove_one_allowlist_rule(permission, scope) {
                removed += 1;
            }
            if permission.is_empty() {
                target.shift_remove("permission");
            }
            Ok((removed > 0, removed))
        }
    }
}

fn permission_object(
    target: &mut Map<String, Value>,
    create: bool,
) -> Result<Option<&mut Map<String, Value>>, SettingsWriteError> {
    if create && !target.contains_key("permission") {
        target.insert("permission".to_owned(), Value::Object(Map::new()));
    }
    match target.get_mut("permission") {
        None => Ok(None),
        Some(Value::Object(permission)) => Ok(Some(permission)),
        Some(_) => Err(SettingsWriteError::InvalidFormat),
    }
}

fn remove_one_allowlist_rule(
    permission: &mut Map<String, Value>,
    scope: AllowlistResetScope,
) -> bool {
    let is_allow = |value: &Value| {
        value
            .as_str()
            .is_some_and(|action| action.eq_ignore_ascii_case("allow"))
    };
    let found = permission
        .iter()
        .filter(|(category, _)| scope.matches_category(category))
        .find_map(|(category, value)| match value {
            Value::String(_) if is_allow(value) => Some((category.clone(), None)),
            Value::Object(rules) => rules
                .iter()
                .find(|(_, action)| is_allow(action))
                .map(|(pattern, _)| (category.clone(), Some(pattern.clone()))),
            _ => None,
        });
    let Some((category, pattern)) = found else {
        return false;
    };
    if let (Some(pattern), Some(Value::Object(rules))) = (pattern, permission.get_mut(&category)) {
        rules.shift_remove(&pattern);
        if !rules.is_empty() {
            return true;
        }
    }
    permission.shift_remove(&category);
    true
}

fn canonical_permission_key(object: &Map<String, Value>, expected: &str) -> Option<String> {
    object
        .keys()
        .find(|key| key.trim_matches([' ', '\t', '\r', '\n']) == expected)
        .cloned()
}

fn migrate_workspace_preference(
    root: &mut Map<String, Value>,
    migration: &Migration,
    application: &mut Application,
) {
    let Some(Value::Object(workspaces)) = root.get_mut("workspaces") else {
        return;
    };
    workspaces.retain(|_, workspace| {
        let Value::Object(workspace) = workspace else {
            return true;
        };
        let field = match migration.container {
            None => workspace.shift_remove(migration.field).is_some(),
            Some(container) => remove_nested_leaf(workspace, container, migration.field),
        };
        let binding = migration
            .binding
            .is_some_and(|binding| workspace.shift_remove(binding).is_some());
        if !field && !binding {
            return true;
        }
        application.changed = true;
        application.fields_removed += usize::from(field) + usize::from(binding);
        application.workspaces_changed += 1;
        if field
            && !application
                .migration_snapshots
                .contains(&migration.snapshot)
        {
            application.migration_snapshots.push(migration.snapshot);
        }
        !workspace.is_empty()
    });
}

fn remove_nested_leaf(workspace: &mut Map<String, Value>, container: &str, leaf: &str) -> bool {
    let Some(Value::Object(object)) = workspace.get_mut(container) else {
        return false;
    };
    if object.shift_remove(leaf).is_none() {
        return false;
    }
    if object.is_empty() {
        workspace.shift_remove(container);
    }
    true
}

fn put_bool(object: &mut Map<String, Value>, key: &str, value: bool) -> bool {
    if object.get(key) == Some(&Value::Bool(value)) {
        return false;
    }
    object.insert(key.to_owned(), Value::Bool(value));
    true
}

fn remove_retired_settings(root: &mut Map<String, Value>) -> bool {
    let mut changed = remove_keys(root, &RETIRED_SETTINGS);
    if let Some(Value::Object(workspaces)) = root.get_mut("workspaces") {
        for workspace in workspaces.values_mut() {
            if let Value::Object(workspace) = workspace {
                changed |= remove_keys(workspace, &RETIRED_SETTINGS);
            }
        }
    }
    changed
}

fn remove_keys(object: &mut Map<String, Value>, keys: &[&str]) -> bool {
    let mut removed = false;
    for key in keys {
        removed |= object.shift_remove(*key).is_some();
    }
    removed
}

fn put_model(
    root: &mut Map<String, Value>,
    provider: &ProviderId,
    model: &str,
) -> Result<bool, SettingsWriteError> {
    if !is_valid_model_id(model) {
        return Err(SettingsWriteError::InvalidField);
    }
    let mut changed = false;
    if !root.contains_key("models") {
        root.insert("models".to_owned(), Value::Object(Map::new()));
        changed = true;
    }
    let Some(Value::Object(models)) = root.get_mut("models") else {
        return Err(SettingsWriteError::InvalidFormat);
    };
    changed |= put_string(models, provider.label(), model);
    if *provider == ProviderId::Codex {
        changed |= root.shift_remove(LEGACY_CODEX_MODEL).is_some();
    } else if *provider == ProviderId::Grok {
        changed |= root.shift_remove("grok_model").is_some();
    }
    Ok(changed)
}

fn put_string(object: &mut Map<String, Value>, key: &str, value: &str) -> bool {
    if object.get(key).and_then(Value::as_str) == Some(value) {
        return false;
    }
    object.insert(key.to_owned(), Value::String(value.to_owned()));
    true
}

fn clear_workspace_fast_mode_bindings(root: &mut Map<String, Value>) {
    let Some(Value::Object(workspaces)) = root.get_mut("workspaces") else {
        return;
    };
    workspaces.retain(|_, workspace| match workspace {
        Value::Object(workspace) => {
            workspace.shift_remove(FAST_MODE_MODEL_BOUND).is_none() || !workspace.is_empty()
        }
        _ => true,
    });
}

fn validate_candidate(candidate: &str, patch: Patch<'_>) -> Result<(), SettingsWriteError> {
    let Ok(Value::Object(root)) = parse_strict_json_value(candidate.as_bytes()) else {
        return Err(SettingsWriteError::InvalidFormat);
    };
    if let Patch::Permission {
        workspace: Some(workspace_root),
        ..
    } = patch
    {
        let workspace = root
            .get("workspaces")
            .and_then(|workspaces| workspaces.get(workspace_root));
        if !matches!(workspace, Some(Value::Object(workspace)) if is_valid_profile_layer(workspace, LayerScope::Workspace))
        {
            return Err(SettingsWriteError::InvalidFormat);
        }
    }
    let canonical = match root.get("models") {
        Some(Value::Object(models)) => models
            .keys()
            .all(|key| ProviderId::parse(key).is_some_and(|provider| provider.label() == key)),
        _ => true,
    };
    if canonical && is_valid_profile_layer(&root, LayerScope::Global) {
        Ok(())
    } else {
        Err(SettingsWriteError::InvalidFormat)
    }
}

fn serialize(root: &Map<String, Value>) -> String {
    let mut out = String::new();
    write_object(&mut out, root);
    out.push('\n');
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Object(object) => write_object(out, object),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Number(number) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
            (Some(integer), _, _) => {
                let _ = write!(out, "{integer}");
            }
            (None, Some(integer), _) => {
                let _ = write!(out, "{integer}");
            }
            (None, None, float) => {
                let _ = write!(out, "{}", float.unwrap_or_default());
            }
        },
        other => out.push_str(&other.to_string()),
    }
}

fn write_object(out: &mut String, object: &Map<String, Value>) {
    out.push('{');
    for (index, (key, value)) in object.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&Value::String(key.clone()).to_string());
        out.push(':');
        write_value(out, value);
    }
    out.push('}');
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsCopy {
    Backup,
    Corrupt,
}

impl SettingsCopy {
    const fn kind(self) -> &'static str {
        match self {
            Self::Backup => "backup",
            Self::Corrupt => "corrupt",
        }
    }

    const fn keep(self) -> usize {
        match self {
            Self::Backup => BACKUP_KEEP_COUNT,
            Self::Corrupt => CORRUPT_KEEP_COUNT,
        }
    }

    fn prefix(self) -> String {
        format!("{SETTINGS_FILE}.{}.", self.kind())
    }
}

fn create_sequenced_copy(
    directory: &PrivateDir,
    copy: SettingsCopy,
    bytes: &[u8],
) -> Result<(), DurableError> {
    let backups = directory.open_or_create_child(BACKUPS_DIRECTORY)?;
    let names = backups.names()?;
    if copy == SettingsCopy::Corrupt && contains_copy(&backups, &names, copy, bytes) {
        return Ok(());
    }
    let sequence = names
        .iter()
        .filter_map(|name| parse_sequence(name))
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| DurableError::Failed)?;
    let mut name = format!(
        "{}{}-{sequence:016x}-",
        copy.prefix(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis())
    );
    for byte in random {
        let _ = write!(name, "{byte:02x}");
    }
    backups.replace(&name, bytes)?;
    prune_copies(&backups, copy, &name);
    Ok(())
}

fn contains_copy(backups: &PrivateDir, names: &[String], copy: SettingsCopy, bytes: &[u8]) -> bool {
    let prefix = copy.prefix();
    names
        .iter()
        .filter(|name| name.starts_with(&prefix))
        .any(|name| {
            backups
                .read_single_link_file(name, MAX_SETTINGS_BYTES)
                .as_deref()
                == Some(bytes)
        })
}

fn prune_copies(backups: &PrivateDir, copy: SettingsCopy, written: &str) {
    let prefix = copy.prefix();
    let Ok(names) = backups.names() else {
        return;
    };
    let copies = newest_first(
        names
            .into_iter()
            .filter(|name| name != written && name.starts_with(&prefix)),
    );
    for name in copies.iter().skip(copy.keep().saturating_sub(1)) {
        if backups.is_single_link_file(name) {
            let _ = backups.remove(name);
        }
    }
}

fn newest_first(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut copies: Vec<(Option<u64>, i64, String)> = names
        .filter_map(|name| Some((parse_sequence(&name), parse_backup_timestamp(&name)?, name)))
        .collect();
    copies.sort_unstable();
    copies.into_iter().rev().map(|(_, _, name)| name).collect()
}

fn copy_suffix(name: &str) -> Option<&str> {
    [SettingsCopy::Backup, SettingsCopy::Corrupt]
        .into_iter()
        .find_map(|copy| name.strip_prefix(&copy.prefix()))
}

fn parse_sequence(name: &str) -> Option<u64> {
    copy_suffix(name)?;
    let start = name.find('-')? + 1;
    let digits = name.get(start..start + 16)?;
    if name.as_bytes().get(start + 16) != Some(&b'-') {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

fn parse_backup_timestamp(name: &str) -> Option<i64> {
    let suffix = copy_suffix(name)?;
    let end = suffix.find('-').unwrap_or(suffix.len());
    if end == 0 {
        return None;
    }
    suffix[..end].parse().ok()
}

pub(crate) fn validate_provider_slug(slug: &str) -> bool {
    slug.len() <= MAX_PROVIDER_SLUG_BYTES
        && slug.starts_with(|first: char| first.is_ascii_alphanumeric())
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests;
