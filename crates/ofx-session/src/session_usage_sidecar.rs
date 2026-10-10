use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;

use ofx_config::PrivateDir;
use ofx_contract::{UsageCompleteness, UsageIncident};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::json_fields::{parse_json, push_string};
use crate::session_error::SessionError;
use crate::session_log::managed_file::{file_type, permissions, private_file_mode};
use crate::session_usage::{MAX_SNAPSHOT_BYTES, UsageSnapshot};

pub(crate) const SIDECAR_FILE: &str = "usage-v2.json";
const MAX_SIDECAR_BYTES: usize = MAX_SNAPSHOT_BYTES + 512;
const SIDECAR_SCHEMA_VERSION: u64 = 1;
const SIDECAR_FIELDS: usize = 3;

enum Captured {
    Missing,
    Unreadable(Damage),
    Encoded(Vec<u8>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Damage {
    Empty,
    Oversized,
    Unsafe,
}

pub(crate) fn write(
    dir: &PrivateDir,
    session_id: &str,
    snapshot: &UsageSnapshot,
) -> Result<(), SessionError> {
    dir.replace(SIDECAR_FILE, &encode(session_id, snapshot)?)?;
    Ok(())
}

fn encode(session_id: &str, snapshot: &UsageSnapshot) -> Result<Vec<u8>, SessionError> {
    let mut encoded = String::from("{\"schema_version\":1,\"session_id\":");
    push_string(&mut encoded, session_id);
    encoded.push_str(",\"snapshot\":");
    snapshot.write_rich(&mut encoded)?;
    encoded.push('}');
    if encoded.len() > MAX_SIDECAR_BYTES {
        return Err(SessionError::UsageSidecarTooLarge);
    }
    Ok(encoded.into_bytes())
}

pub(crate) fn load_conversation(
    dir: &PrivateDir,
    session_id: &str,
    continuity_at_ms: i64,
) -> Result<UsageSnapshot, SessionError> {
    let restored = match capture(dir) {
        Captured::Unreadable(Damage::Unsafe) => return Err(SessionError::InvalidUsageSidecar),
        Captured::Encoded(bytes) => decode(&bytes, session_id),
        Captured::Missing | Captured::Unreadable(Damage::Empty | Damage::Oversized) => None,
    };
    if let Some(snapshot) = restored {
        return Ok(snapshot);
    }
    let mut snapshot = UsageSnapshot::unavailable();
    snapshot.append_incident(UsageIncident {
        occurred_at_ms: continuity_at_ms.max(0),
        completeness: UsageCompleteness::Incomplete,
    })?;
    Ok(snapshot)
}

fn capture(dir: &PrivateDir) -> Captured {
    let initial = match fs::statat(dir.as_fd(), SIDECAR_FILE, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Captured::Missing,
        Err(_) => return Captured::Unreadable(Damage::Unsafe),
    };
    if !private_single_link_file(&initial) {
        return Captured::Unreadable(Damage::Unsafe);
    }
    let Ok(size) = usize::try_from(initial.st_size) else {
        return Captured::Unreadable(Damage::Oversized);
    };
    if size == 0 {
        return Captured::Unreadable(Damage::Empty);
    }
    if size > MAX_SIDECAR_BYTES {
        return Captured::Unreadable(Damage::Oversized);
    }
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let file = match fs::openat(dir.as_fd(), SIDECAR_FILE, flags, Mode::empty()) {
        Ok(file) => file,
        Err(Errno::NOENT) => return Captured::Missing,
        Err(_) => return Captured::Unreadable(Damage::Unsafe),
    };
    let unchanged = fs::fstat(&file).is_ok_and(|verified| {
        private_single_link_file(&verified) && verified.st_size == initial.st_size
    });
    if !unchanged {
        return Captured::Unreadable(Damage::Unsafe);
    }
    let mut bytes = Vec::with_capacity(size);
    let limit = u64::try_from(MAX_SIDECAR_BYTES + 1).unwrap_or(u64::MAX);
    if File::from(file)
        .take(limit)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_SIDECAR_BYTES
    {
        return Captured::Unreadable(Damage::Unsafe);
    }
    Captured::Encoded(bytes)
}

fn private_single_link_file(stat: &Stat) -> bool {
    file_type(stat) == FileType::RegularFile
        && stat.st_nlink == 1
        && permissions(stat) == private_file_mode()
}

fn decode(bytes: &[u8], session_id: &str) -> Option<UsageSnapshot> {
    if bytes.is_empty() || bytes.len() > MAX_SIDECAR_BYTES {
        return None;
    }
    let value = parse_json(bytes).ok()?;
    let object = value.as_object()?;
    if object.len() != SIDECAR_FIELDS
        || object.get("schema_version")?.as_u64()? != SIDECAR_SCHEMA_VERSION
    {
        return None;
    }
    let bound_id = object.get("session_id")?.as_str()?;
    let snapshot = object.get("snapshot")?;
    if bound_id.is_empty() || snapshot.get("schema_version").is_none() {
        return None;
    }
    let snapshot = UsageSnapshot::parse_rich(snapshot).ok()?;
    (bound_id == session_id).then_some(snapshot)
}

#[cfg(test)]
mod tests;
