use std::fmt::Write as _;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::config_runtime::{
    BYTE_ORDER_MARK, MAX_SETTINGS_BYTES, SETTINGS_FILE, is_valid_profile_layer,
};
use crate::configured_provider::is_valid_model_id;
use crate::io::{AdvisoryLock, DurableError, PrivateDir};
use crate::model_provider::ProviderId;
use crate::paths::ProfilePaths;
use crate::strict_json;

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
const FAST_MODE_MODEL_BOUND: &str = "fast_mode_model_bound";
const LEGACY_CODEX_MODEL: &str = "codex_model";

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

pub fn save_codex_model(paths: &ProfilePaths, model: &str) -> Result<(), SettingsWriteError> {
    save_codex_model_with(paths, model, &mut || {})
}

fn save_codex_model_with(
    paths: &ProfilePaths,
    model: &str,
    before_commit: &mut dyn FnMut(),
) -> Result<(), SettingsWriteError> {
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
        if !apply_codex_model(&mut root, model)? {
            return Ok(());
        }
        if existing.as_deref().is_some_and(has_unpreserved_number) {
            return Err(SettingsWriteError::NumberNotPreserved);
        }
        let candidate = serialize(&root);
        if candidate.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsWriteError::TooLarge);
        }
        validate_candidate(&candidate)?;
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
        directory.replace(SETTINGS_FILE, candidate.as_bytes())?;
        return Ok(());
    }
    Err(SettingsWriteError::ConcurrentModification)
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
    match strict_json::parse(bytes) {
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

fn apply_codex_model(
    root: &mut Map<String, Value>,
    model: &str,
) -> Result<bool, SettingsWriteError> {
    let mut changed = remove_retired_settings(root);
    changed |= put_codex_model(root, model)?;
    changed |= put_string(root, "provider", ProviderId::Codex.label());
    if root.shift_remove(FAST_MODE_MODEL_BOUND).is_some() {
        changed = true;
    }
    clear_workspace_fast_mode_bindings(root)?;
    Ok(changed)
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

fn put_codex_model(root: &mut Map<String, Value>, model: &str) -> Result<bool, SettingsWriteError> {
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
    changed |= put_string(models, ProviderId::Codex.label(), model);
    changed |= root.shift_remove(LEGACY_CODEX_MODEL).is_some();
    Ok(changed)
}

fn put_string(object: &mut Map<String, Value>, key: &str, value: &str) -> bool {
    if object.get(key).and_then(Value::as_str) == Some(value) {
        return false;
    }
    object.insert(key.to_owned(), Value::String(value.to_owned()));
    true
}

fn clear_workspace_fast_mode_bindings(
    root: &mut Map<String, Value>,
) -> Result<(), SettingsWriteError> {
    let Some(workspaces) = root.get_mut("workspaces") else {
        return Ok(());
    };
    let Value::Object(workspaces) = workspaces else {
        return Err(SettingsWriteError::InvalidFormat);
    };
    workspaces.retain(|_, workspace| match workspace {
        Value::Object(workspace) => {
            workspace.shift_remove(FAST_MODE_MODEL_BOUND).is_none() || !workspace.is_empty()
        }
        _ => true,
    });
    Ok(())
}

fn validate_candidate(candidate: &str) -> Result<(), SettingsWriteError> {
    let Ok(Value::Object(root)) = strict_json::parse(candidate.as_bytes()) else {
        return Err(SettingsWriteError::InvalidFormat);
    };
    let canonical = match root.get("models") {
        Some(Value::Object(models)) => models
            .keys()
            .all(|key| ProviderId::parse(key).is_some_and(|provider| provider.label() == key)),
        _ => true,
    };
    if canonical && is_valid_profile_layer(&root) {
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
    let mut names: Vec<String> = names
        .into_iter()
        .filter(|name| {
            name != written && name.starts_with(&prefix) && parse_backup_timestamp(name).is_some()
        })
        .collect();
    names.sort_by(|left, right| {
        if backup_name_newer_than(left, right) {
            std::cmp::Ordering::Less
        } else if backup_name_newer_than(right, left) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    for name in names.iter().skip(copy.keep().saturating_sub(1)) {
        if backups.is_single_link_file(name) {
            let _ = backups.remove(name);
        }
    }
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

fn backup_name_newer_than(left: &str, right: &str) -> bool {
    match (parse_sequence(left), parse_sequence(right)) {
        (Some(left), Some(right)) if left != right => return left > right,
        (Some(_), None) => return true,
        (None, Some(_)) => return false,
        _ => {}
    }
    match (parse_backup_timestamp(left), parse_backup_timestamp(right)) {
        (Some(left), Some(right)) if left != right => left > right,
        _ => left > right,
    }
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
