use std::fs::{File, Metadata};
use std::os::unix::ffi::OsStrExt;

use ofx_config::{ContextLimit, line_safe_prefix_length};
use ofx_text::{InvalidUtf8, Utf8Validator};
use ofx_workspace::{PathError, basename};
use tokio_util::sync::CancellationToken;

use super::SkillError;
use crate::io::{FileFreshness, read_positional_all};
use crate::skill_contract::{MAX_FRONTMATTER_BYTES, Skill, parse_skill_file, resolve_metadata};
use crate::skill_runtime::{OpenedSkillCandidate, ResourceOpenError, resource_is_skill_file};

const VALIDATION_CHUNK_BYTES: usize = 16 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;
const UTF8_LOOKAHEAD_BYTES: usize = 3;
const IDENTITY_WINDOW_BYTES: usize = MAX_FRONTMATTER_BYTES + 1;

#[derive(Debug)]
pub(crate) struct SkillResourceRead {
    pub(crate) text: String,
    pub(crate) observed_bytes: usize,
}

pub(crate) fn check_cancelled(cancellation: Option<&CancellationToken>) -> Result<(), SkillError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        Err(SkillError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn read_skill_resource(
    candidate: &OpenedSkillCandidate,
    resource: &str,
    limit: ContextLimit,
    safety_ceiling: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<SkillResourceRead, SkillError> {
    if resource_is_skill_file(resource) {
        return read_skill_file(
            candidate.skill_file(),
            candidate.freshness(),
            limit,
            safety_ceiling,
            cancellation,
        );
    }
    let (file, opened) = candidate
        .open_resource(resource)
        .map_err(|error| match error {
            ResourceOpenError::InvalidPath => SkillError::InvalidSkillResourcePath,
            ResourceOpenError::NotRegularFile => SkillError::InvalidSkillResource,
            ResourceOpenError::Path(error) => SkillError::Path(error),
        })?;
    read_skill_file(&file, opened, limit, safety_ceiling, cancellation)
}

pub(crate) fn read_skill_file(
    file: &File,
    expected: FileFreshness,
    limit: ContextLimit,
    safety_ceiling: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<SkillResourceRead, SkillError> {
    let before = unchanged_metadata(file, expected)?;
    if !before.is_file() {
        return Err(SkillError::InvalidSkillResource);
    }
    let observed_bytes =
        usize::try_from(before.len()).map_err(|_| SkillError::SkillFileLimitExceeded)?;
    let effective_limit = limit.effective_bytes();
    validate_utf8(
        file,
        observed_bytes.min(safety_ceiling),
        observed_bytes,
        cancellation,
    )?;

    let read_len = observed_bytes.min(
        effective_limit
            .saturating_add(UTF8_LOOKAHEAD_BYTES)
            .min(safety_ceiling),
    );
    let mut bytes = vec![0; read_len];
    let mut bytes_read = 0;
    while bytes_read < read_len {
        check_cancelled(cancellation)?;
        let end = bytes_read + (read_len - bytes_read).min(READ_CHUNK_BYTES);
        let count = read_at(file, &mut bytes[bytes_read..end], bytes_read)?;
        if count == 0 {
            break;
        }
        bytes_read += count;
    }
    unchanged_metadata(file, expected)?;
    if bytes_read != read_len {
        return Err(SkillError::UnexpectedEndOfFile);
    }
    bytes.truncate(line_safe_prefix_length(&bytes, effective_limit));
    let text = String::from_utf8(bytes).map_err(|_| SkillError::BinarySkillResource)?;
    Ok(SkillResourceRead {
        text,
        observed_bytes,
    })
}

fn validate_utf8(
    file: &File,
    byte_count: usize,
    observed_bytes: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<(), SkillError> {
    let mut chunk = vec![0; VALIDATION_CHUNK_BYTES];
    let mut validator = Utf8Validator::default();
    let mut read_offset = 0;
    while read_offset < byte_count {
        check_cancelled(cancellation)?;
        let wanted = VALIDATION_CHUNK_BYTES.min(byte_count - read_offset);
        let read = &mut chunk[..wanted];
        if read_at(file, read, read_offset)? != wanted {
            return Err(SkillError::UnexpectedEndOfFile);
        }
        validator.push(read).map_err(binary)?;
        read_offset += wanted;
    }
    let lookahead_len = validator
        .missing_bytes()
        .min(observed_bytes.saturating_sub(byte_count));
    if lookahead_len > 0 {
        let lookahead = &mut chunk[..lookahead_len];
        if read_at(file, lookahead, byte_count)? != lookahead_len {
            return Err(SkillError::UnexpectedEndOfFile);
        }
        validator.push(lookahead).map_err(binary)?;
    }
    validator.finish().map_err(binary)
}

fn binary(_: InvalidUtf8) -> SkillError {
    SkillError::BinarySkillResource
}

pub(crate) fn verify_read_identity(
    candidate: &OpenedSkillCandidate,
    skill: &Skill,
    read: &SkillResourceRead,
) -> Result<(), SkillError> {
    if read.text.len() >= read.observed_bytes.min(IDENTITY_WINDOW_BYTES) {
        verify_identity(read.text.as_bytes(), skill)
    } else {
        revalidate_primary_identity(candidate, skill)
    }
}

pub(crate) fn revalidate_primary_identity(
    candidate: &OpenedSkillCandidate,
    skill: &Skill,
) -> Result<(), SkillError> {
    let file = candidate.skill_file();
    let before = unchanged_metadata(file, candidate.freshness())?;
    let length = usize::try_from(before.len())
        .unwrap_or(usize::MAX)
        .min(IDENTITY_WINDOW_BYTES);
    let mut bytes = vec![0; length];
    if read_at(file, &mut bytes, 0)? != length {
        return Err(SkillError::SkillResourceChanged);
    }
    unchanged_metadata(file, candidate.freshness())?;
    verify_identity(&bytes, skill)
}

fn verify_identity(content: &[u8], skill: &Skill) -> Result<(), SkillError> {
    let fallback_name = basename(skill.path.as_os_str().as_bytes());
    match resolve_metadata(&parse_skill_file(content), fallback_name) {
        Ok(metadata) if metadata.name == skill.name => Ok(()),
        _ => Err(SkillError::SkillResourceChanged),
    }
}

fn unchanged_metadata(file: &File, expected: FileFreshness) -> Result<Metadata, SkillError> {
    let current = file
        .metadata()
        .map_err(|error| SkillError::Path(PathError::from(error)))?;
    if FileFreshness::of(&current) == expected {
        Ok(current)
    } else {
        Err(SkillError::SkillResourceChanged)
    }
}

fn read_at(file: &File, buffer: &mut [u8], offset: usize) -> Result<usize, SkillError> {
    let offset = u64::try_from(offset).map_err(|_| SkillError::SkillFileLimitExceeded)?;
    read_positional_all(file, buffer, offset)
        .map_err(|error| SkillError::Path(PathError::from(error)))
}
