use ofx_config::PrivateDir;

use crate::json_fields::{Json, parse_json};
use crate::session_error::SessionError;
use crate::session_layout::is_valid_session_id;
use crate::session_log::managed_file::{entry_exists, read_managed_file};

const AUTHORITY_FILE: &str = "authority.json";
const AUTHORITY_FENCE_FILE: &str = "authority.pending.json";
pub(crate) const MAX_CONTROL_FILE_BYTES: usize = 16 * 1024;
const MARKER_SCHEMA_VERSION: u64 = 1;
const STORAGE_FORMAT: &str = "event_log_v1";
const SOURCES: [&str; 2] = ["native_create", "legacy_migration"];
pub(crate) type Identifier = [u8; 16];

pub(crate) fn holds_authority_marker(dir: &PrivateDir) -> Result<bool, SessionError> {
    entry_exists(dir, AUTHORITY_FILE)
}

pub(crate) fn require_schema_v3(dir: &PrivateDir, id: &str) -> Result<(), SessionError> {
    if entry_exists(dir, AUTHORITY_FENCE_FILE)? {
        return Err(SessionError::InvalidSessionFormat);
    }
    let bytes = read_managed_file(dir, AUTHORITY_FILE, MAX_CONTROL_FILE_BYTES)?
        .ok_or(SessionError::InvalidSessionFormat)?;
    let marker = parse_json(&bytes).map_err(|_| SessionError::InvalidSessionFormat)?;
    if names_session(&marker, id) {
        Ok(())
    } else {
        Err(SessionError::InvalidSessionFormat)
    }
}

fn names_session(marker: &Json<'_>, id: &str) -> bool {
    let text = |key| marker.get(key).and_then(Json::as_str);
    marker.get("schema_version").and_then(Json::as_u64) == Some(MARKER_SCHEMA_VERSION)
        && text("storage_format") == Some(STORAGE_FORMAT)
        && text("source").is_some_and(|source| SOURCES.contains(&source))
        && text("authority_id").and_then(parse_identifier).is_some()
        && text("session_id").is_some_and(|session| is_valid_session_id(session) && session == id)
}

pub(crate) fn parse_identifier(hex: &str) -> Option<Identifier> {
    parse_hex(hex)
}

pub(crate) fn parse_hex<const BYTES: usize>(hex: &str) -> Option<[u8; BYTES]> {
    let lowercase = hex
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !lowercase || hex.len() != BYTES * 2 {
        return None;
    }
    let mut bytes = [0_u8; BYTES];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests;
