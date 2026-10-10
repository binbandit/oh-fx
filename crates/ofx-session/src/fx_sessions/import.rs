use std::io;
use std::os::fd::{AsFd, OwnedFd};

use ofx_config::{AdvisoryLock, PrivateDir};
use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use serde_json::{Value, json};

use crate::session_children::CONTROL_DIR;
use crate::session_codec::SessionPreferences;
use crate::session_discovery::{Classification, classify_session};
use crate::session_error::SessionError;
use crate::session_log::managed_file::{
    Access, create_managed_file, create_private_dir, file_type, open_managed_file, publish_dir,
    read_managed_file, remove_created_dir, sync_dir,
};
use crate::session_log::{
    EVENTS_FILE, MANIFEST_FILE, SESSION_LOCK_FILE, read_metadata, staging_name,
};
use crate::session_migration::{Converted, holds_schema_v3, read_schema_v3, schema_v3_watermark};

const IMPORT_MARKER: &str = "fx-import.json";
const MARKER_VERSION: u64 = 1;
const MAX_MARKER_BYTES: usize = 4 * 1024;
const COPIED_FILES: [&str; 6] = [
    MANIFEST_FILE,
    EVENTS_FILE,
    RECOVERY_FILE,
    "recovery.asked",
    "permissions.json",
    "usage-v2.json",
];
const COPIED_DIRS: [&str; 4] = ["tool-results", "images", "logs", "artifacts"];
const UNFINISHED_COMPACTION: [&str; 2] = ["events.jsonl.compact-tmp", "events-compaction.pending"];
const RECOVERY_FILE: &str = "recovery.json";
const NESTED_DIRS: usize = 1;
const NANOS_PER_SECOND: i128 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    inode: u64,
    size: u64,
    modified_ns: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImportSource {
    manifest: Option<FileStamp>,
    events: FileStamp,
    watermark: Option<FileStamp>,
}

pub(crate) struct Imported {
    pub(crate) source: ImportSource,
    pub(crate) copy: PrivateDir,
    pub(crate) lock: AdvisoryLock,
}

enum Contents {
    Current,
    Converted(Box<Converted>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyState {
    log_bytes: u64,
    recovery: Option<FileStamp>,
    title: Option<String>,
    preferences: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marker {
    source: ImportSource,
    copy: CopyState,
}

pub(crate) fn seal(copy: &PrivateDir, id: &str, source: ImportSource) -> Result<(), SessionError> {
    let state = copy_state(copy, id)?;
    let marker = json!({
        "version": MARKER_VERSION,
        "source": {
            "session_json": source.manifest.map(stamp_json),
            "events_jsonl": stamp_json(source.events),
            "commit_json": source.watermark.map(stamp_json),
        },
        "copy": {
            "log_bytes": state.log_bytes,
            "recovery_json": state.recovery.map(stamp_json),
            "title": state.title,
            "preferences": state.preferences,
        },
    });
    Ok(copy.replace(IMPORT_MARKER, marker.to_string().as_bytes())?)
}

pub(crate) fn import(
    fx: &PrivateDir,
    sessions: &PrivateDir,
    id: &str,
) -> Result<Option<Imported>, SessionError> {
    let Some(source) = fx.open_child(id).ok().flatten() else {
        return Ok(None);
    };
    let (staging, imported) = stage(fx, &source, sessions, id)?;
    match publish_dir(sessions, &staging, id) {
        Ok(()) => {
            sync_dir(sessions)?;
            Ok(Some(imported))
        }
        Err(SessionError::SessionAlreadyExists) => {
            remove_created_dir(sessions, &staging);
            Ok(None)
        }
        Err(error) => {
            remove_created_dir(sessions, &staging);
            Err(error)
        }
    }
}

pub(crate) fn refresh(
    fx: &PrivateDir,
    sessions: &PrivateDir,
    copy: &PrivateDir,
    id: &str,
    marker: &Marker,
) -> Result<Option<Imported>, SessionError> {
    let Some(source) = fx.open_child(id).ok().flatten() else {
        return Ok(None);
    };
    if !stale(&source, copy, id, marker) {
        return Ok(None);
    }
    let Ok(Some(_held)) = copy.try_lock(SESSION_LOCK_FILE) else {
        return Ok(None);
    };
    if !untouched(copy, id, marker) {
        return Ok(None);
    }
    let (staging, imported) = match stage(fx, &source, sessions, id) {
        Ok(staged) => staged,
        Err(SessionError::FxSessionOpen) => return Err(SessionError::FxSessionOpen),
        Err(_) => return Ok(None),
    };
    replace(sessions, id, &staging)?;
    Ok(Some(imported))
}

fn stage(
    fx: &PrivateDir,
    source: &PrivateDir,
    sessions: &PrivateDir,
    id: &str,
) -> Result<(String, Imported), SessionError> {
    let _shared = share_lock(source)?;
    for name in UNFINISHED_COMPACTION {
        if present(source, name)? {
            return Err(SessionError::FxCompactionUnfinished);
        }
    }
    let before = source_stamp(source).map_err(|_| SessionError::FxSessionUnreadable)?;
    let contents = contents(fx, source, id)?;
    let staging = staging_name()?;
    create_private_dir(sessions, &staging).map_err(|_| SessionError::SessionStartFailed)?;
    match owned_copy(source, sessions, &staging, before, &contents) {
        Ok(imported) => Ok((staging, imported)),
        Err(error) => {
            remove_created_dir(sessions, &staging);
            Err(error)
        }
    }
}

fn contents(fx: &PrivateDir, source: &PrivateDir, id: &str) -> Result<Contents, SessionError> {
    let unreadable = |_| SessionError::FxSessionUnreadable;
    if holds_schema_v3(source, id).map_err(unreadable)? {
        return match read_schema_v3(source, id).map_err(unreadable)? {
            Some(converted) => Ok(Contents::Converted(Box::new(converted))),
            None => Err(SessionError::OneOffSessionNotResumable),
        };
    }
    match classify_session(fx, id, Classification::Resume) {
        Ok(Some(_)) => Ok(Contents::Current),
        Ok(None) => Err(SessionError::OneOffSessionNotResumable),
        Err(_) => Err(SessionError::FxSessionUnreadable),
    }
}

fn owned_copy(
    source: &PrivateDir,
    sessions: &PrivateDir,
    staging: &str,
    before: ImportSource,
    contents: &Contents,
) -> Result<Imported, SessionError> {
    let copy = sessions
        .open_child_private(staging)?
        .ok_or(SessionError::SessionStartFailed)?;
    match contents {
        Contents::Current => copy_session(source, &copy)?,
        Contents::Converted(converted) => {
            copy_dirs(source, &copy)?;
            converted.write(&copy)?;
        }
    }
    if source_stamp(source)? != before {
        return Err(SessionError::FxSessionOpen);
    }
    let lock = copy
        .try_lock(SESSION_LOCK_FILE)?
        .ok_or(SessionError::SessionStartFailed)?;
    Ok(Imported {
        source: before,
        copy,
        lock,
    })
}

fn replace(sessions: &PrivateDir, id: &str, staging: &str) -> Result<(), SessionError> {
    let retired = match staging_name() {
        Ok(retired) => retired,
        Err(error) => {
            remove_created_dir(sessions, staging);
            return Err(error);
        }
    };
    if let Err(error) = publish_dir(sessions, id, &retired) {
        remove_created_dir(sessions, staging);
        return Err(error);
    }
    if let Err(error) = publish_dir(sessions, staging, id) {
        let _ = publish_dir(sessions, &retired, id);
        remove_created_dir(sessions, staging);
        return Err(error);
    }
    remove_created_dir(sessions, &retired);
    sync_dir(sessions)
}

fn share_lock(source: &PrivateDir) -> Result<Option<OwnedFd>, SessionError> {
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let lock = match fs::openat(source.as_fd(), SESSION_LOCK_FILE, flags, Mode::empty()) {
        Ok(lock) => lock,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::ISDIR | Errno::NOTDIR | Errno::NXIO) => {
            return Err(SessionError::SessionPathUnsafe);
        }
        Err(errno) => return Err(errno.into()),
    };
    if file_type(&fs::fstat(&lock)?) != FileType::RegularFile {
        return Err(SessionError::SessionPathUnsafe);
    }
    match fs::flock(&lock, FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(Some(lock)),
        Err(Errno::WOULDBLOCK | Errno::INTR) => Err(SessionError::FxSessionOpen),
        Err(Errno::NOLCK | Errno::OPNOTSUPP) => Err(SessionError::SessionLockUnsupported),
        Err(errno) => Err(errno.into()),
    }
}

fn present(dir: &PrivateDir, name: &str) -> Result<bool, SessionError> {
    match fs::statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(Errno::NOENT) => Ok(false),
        Err(errno) => Err(errno.into()),
    }
}

fn file_stamp(dir: &PrivateDir, name: &str) -> Result<FileStamp, SessionError> {
    let stat = fs::statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)?;
    let seconds: i128 = widened(stat.st_mtime);
    let nanos: i128 = widened(stat.st_mtime_nsec);
    let modified_ns = seconds
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(nanos);
    Ok(FileStamp {
        inode: widened(stat.st_ino),
        size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
        modified_ns: i64::try_from(modified_ns).unwrap_or(i64::MAX),
    })
}

fn widened<Field: Into<Wide>, Wide>(value: Field) -> Wide {
    value.into()
}

fn source_stamp(source: &PrivateDir) -> Result<ImportSource, SessionError> {
    let manifest = if present(source, MANIFEST_FILE)? {
        Some(file_stamp(source, MANIFEST_FILE)?)
    } else {
        None
    };
    let watermark = match schema_v3_watermark(source)? {
        Some(name) if present(source, &name)? => Some(file_stamp(source, &name)?),
        _ => None,
    };
    Ok(ImportSource {
        manifest,
        events: file_stamp(source, EVENTS_FILE)?,
        watermark,
    })
}

pub(crate) fn outdated(fx: &PrivateDir, copy: &PrivateDir, id: &str) -> bool {
    match (read_marker(copy), fx.open_child(id).ok().flatten()) {
        (Some(marker), Some(source)) => stale(&source, copy, id, &marker),
        _ => false,
    }
}

fn stale(source: &PrivateDir, copy: &PrivateDir, id: &str, marker: &Marker) -> bool {
    !source_stamp(source).is_ok_and(|current| current == marker.source)
        && untouched(copy, id, marker)
}

pub(crate) fn untouched_import(copy: &PrivateDir, id: &str) -> Option<ImportSource> {
    let marker = read_marker(copy)?;
    untouched(copy, id, &marker).then_some(marker.source)
}

fn untouched(copy: &PrivateDir, id: &str, marker: &Marker) -> bool {
    present(copy, CONTROL_DIR).is_ok_and(|children| !children)
        && copy_state(copy, id).is_ok_and(|state| state == marker.copy)
}

fn copy_state(copy: &PrivateDir, id: &str) -> Result<CopyState, SessionError> {
    let recovery = if present(copy, RECOVERY_FILE)? {
        Some(file_stamp(copy, RECOVERY_FILE)?)
    } else {
        None
    };
    let metadata = read_metadata(copy, id)?;
    Ok(CopyState {
        log_bytes: file_stamp(copy, EVENTS_FILE)?.size,
        recovery,
        title: metadata.title,
        preferences: preferences_json(&metadata.preferences),
    })
}

fn preferences_json(preferences: &SessionPreferences) -> Value {
    let SessionPreferences {
        provider,
        model,
        effort,
        fast_mode,
        ultrafast_mode,
    } = preferences;
    let mut recorded = json!({
        "provider": provider,
        "model": model,
        "effort": effort.label(),
        "fast_mode": fast_mode,
    });
    if *ultrafast_mode {
        recorded["ultrafast_mode"] = Value::Bool(true);
    }
    recorded
}

fn copy_session(source: &PrivateDir, target: &PrivateDir) -> Result<(), SessionError> {
    for name in COPIED_FILES {
        copy_file(source, target, name)?;
    }
    copy_dirs(source, target)?;
    sync_dir(target)
}

fn copy_dirs(source: &PrivateDir, target: &PrivateDir) -> Result<(), SessionError> {
    for name in COPIED_DIRS {
        if let Some(dir) = child_dir(source, name)? {
            copy_tree(&dir, target, name, NESTED_DIRS)?;
        }
    }
    Ok(())
}

fn child_dir(parent: &PrivateDir, name: &str) -> Result<Option<PrivateDir>, SessionError> {
    match parent.open_child(name) {
        Ok(dir) => Ok(dir),
        Err(ofx_config::DurableError::PathUnsafe) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn copy_file(source: &PrivateDir, target: &PrivateDir, name: &str) -> Result<(), SessionError> {
    let Some(mut from) = open_managed_file(source, name, Access::ReadOnly)? else {
        return Ok(());
    };
    let mut to = create_managed_file(target, name)?;
    io::copy(&mut from, &mut to)?;
    to.sync_all()?;
    Ok(())
}

fn copy_tree(
    source: &PrivateDir,
    target_parent: &PrivateDir,
    name: &str,
    nested: usize,
) -> Result<(), SessionError> {
    create_private_dir(target_parent, name)?;
    let target = target_parent
        .open_child_private(name)?
        .ok_or(SessionError::SessionStartFailed)?;
    for (entry, kind) in entries(source)? {
        match kind {
            FileType::RegularFile => copy_file(source, &target, &entry)?,
            FileType::Directory if nested > 0 => {
                if let Some(child) = child_dir(source, &entry)? {
                    copy_tree(&child, &target, &entry, nested - 1)?;
                }
            }
            _ => {}
        }
    }
    sync_dir(&target)
}

fn entries(dir: &PrivateDir) -> Result<Vec<(String, FileType)>, SessionError> {
    let mut found = Vec::new();
    for entry in fs::Dir::read_from(dir.as_fd())? {
        let entry = entry?;
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if name == "." || name == ".." {
            continue;
        }
        let kind = match entry.file_type() {
            FileType::Unknown => fs::statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
                .map_or(FileType::Unknown, |stat| file_type(&stat)),
            known => known,
        };
        found.push((name.to_owned(), kind));
    }
    Ok(found)
}

pub(crate) fn read_marker(copy: &PrivateDir) -> Option<Marker> {
    let bytes = read_managed_file(copy, IMPORT_MARKER, MAX_MARKER_BYTES).ok()??;
    let marker: Value = serde_json::from_slice(&bytes).ok()?;
    if marker.get("version")?.as_u64()? != MARKER_VERSION {
        return None;
    }
    let source = marker.get("source")?;
    let copy = marker.get("copy")?;
    let recovery = match copy.get("recovery_json")? {
        Value::Null => None,
        stamp => Some(stamp_from(stamp)?),
    };
    let title = match copy.get("title")? {
        Value::Null => None,
        title => Some(title.as_str()?.to_owned()),
    };
    Some(Marker {
        source: ImportSource {
            manifest: match source.get("session_json")? {
                Value::Null => None,
                stamp => Some(stamp_from(stamp)?),
            },
            events: stamp_from(source.get("events_jsonl")?)?,
            watermark: match source.get("commit_json") {
                None | Some(Value::Null) => None,
                Some(stamp) => Some(stamp_from(stamp)?),
            },
        },
        copy: CopyState {
            log_bytes: copy.get("log_bytes")?.as_u64()?,
            recovery,
            title,
            preferences: copy.get("preferences")?.clone(),
        },
    })
}

fn stamp_json(stamp: FileStamp) -> Value {
    json!([stamp.inode, stamp.size, stamp.modified_ns])
}

fn stamp_from(value: &Value) -> Option<FileStamp> {
    let [inode, size, modified_ns] = value.as_array()?.as_slice() else {
        return None;
    };
    Some(FileStamp {
        inode: inode.as_u64()?,
        size: size.as_u64()?,
        modified_ns: modified_ns.as_i64()?,
    })
}

#[cfg(test)]
mod tests;
