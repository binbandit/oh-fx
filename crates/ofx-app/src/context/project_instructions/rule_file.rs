use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use ofx_config::{EMERGENCY_CEILING_BYTES, line_safe_prefix_length};
use ofx_text::Utf8Validator;
use ofx_workspace::{
    PathError, RegularFileError, open_directory, open_regular_file, open_regular_file_at,
    path_inside,
};

use super::omissions::OmissionReason;
use super::{file_name, parent_directory};

const VALIDATION_CHUNK_BYTES: usize = 16 * 1024;
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RuleLoad {
    Missing,
    Blank,
    Body(RuleBody),
    Omitted(OmissionReason),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RuleBody {
    pub(super) text: String,
    pub(super) observed_bytes: usize,
}

pub(super) fn load_rule(path: &Path, limit_bytes: usize) -> RuleLoad {
    match open_rule(path) {
        Ok((file, size)) => read_rule(&file, size, limit_bytes),
        Err(outcome) => outcome,
    }
}

fn open_rule(path: &Path) -> Result<(File, u64), RuleLoad> {
    let entry = fs::symlink_metadata(path)
        .map_err(entry_failure)?
        .file_type();
    let Some(parent) = parent_directory(path) else {
        return Err(RuleLoad::Missing);
    };
    if entry.is_symlink() {
        return open_contained_symlink(parent, path)
            .ok_or(RuleLoad::Omitted(OmissionReason::Symlink));
    }
    if !entry.is_file() {
        return Err(RuleLoad::Omitted(OmissionReason::NonRegular));
    }
    let directory = open_directory(parent).map_err(lookup_failure)?;
    open_regular_file_at(directory, file_name(path))
        .map(|(file, metadata)| (file, metadata.len()))
        .map_err(regular_failure)
}

pub(super) fn open_contained_symlink(parent: &Path, path: &Path) -> Option<(File, u64)> {
    let authority = fs::canonicalize(parent).ok()?;
    let target = fs::canonicalize(path).ok()?;
    if authority != parent || !path_inside(&authority, &target) {
        return None;
    }
    let (handle, metadata) = open_regular_file(&target).ok()?;
    Some((handle, metadata.len()))
}

fn entry_failure(error: io::Error) -> RuleLoad {
    match PathError::from(error) {
        PathError::FileNotFound | PathError::NotDir => RuleLoad::Missing,
        _ => RuleLoad::Omitted(OmissionReason::Unreadable),
    }
}

fn lookup_failure(error: PathError) -> RuleLoad {
    match error {
        PathError::FileNotFound | PathError::NotDir | PathError::SymLinkLoop => RuleLoad::Missing,
        _ => RuleLoad::Omitted(OmissionReason::Unreadable),
    }
}

fn regular_failure(error: RegularFileError) -> RuleLoad {
    match error {
        RegularFileError::NotRegularFile => RuleLoad::Omitted(OmissionReason::NonRegular),
        RegularFileError::Path(error) => lookup_failure(error),
    }
}

fn read_rule(file: &File, size: u64, limit_bytes: usize) -> RuleLoad {
    let Some(observed_bytes) = usize::try_from(size)
        .ok()
        .filter(|bytes| *bytes <= EMERGENCY_CEILING_BYTES)
    else {
        return RuleLoad::Omitted(OmissionReason::Oversized);
    };
    match validate_utf8_content(file, observed_bytes) {
        Ok(true) => {}
        Ok(false) => return RuleLoad::Blank,
        Err(()) => return RuleLoad::Omitted(OmissionReason::Unreadable),
    }
    let read_len = observed_bytes.min(limit_bytes.saturating_add(3).min(EMERGENCY_CEILING_BYTES));
    let mut content = vec![0; read_len];
    if file.read_exact_at(&mut content, 0).is_err() {
        return RuleLoad::Omitted(OmissionReason::Unreadable);
    }
    content.truncate(line_safe_prefix_length(&content, limit_bytes));
    let Ok(prefix) = String::from_utf8(content) else {
        return RuleLoad::Omitted(OmissionReason::Unreadable);
    };
    RuleLoad::Body(RuleBody {
        text: prefix.trim_matches(TRIMMED).to_owned(),
        observed_bytes,
    })
}

fn validate_utf8_content(file: &File, byte_count: usize) -> Result<bool, ()> {
    let mut chunk = vec![0; VALIDATION_CHUNK_BYTES];
    let mut validator = Utf8Validator::default();
    let mut has_content = false;
    let mut offset = 0;
    while offset < byte_count {
        let wanted = VALIDATION_CHUNK_BYTES.min(byte_count - offset);
        let read = &mut chunk[..wanted];
        file.read_exact_at(read, offset as u64).map_err(drop)?;
        has_content |= read.iter().any(|byte| !b" \t\r\n".contains(byte));
        validator.push(read).map_err(drop)?;
        offset += wanted;
    }
    validator.finish().map_err(drop)?;
    Ok(has_content)
}
