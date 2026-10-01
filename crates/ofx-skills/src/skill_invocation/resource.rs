use std::fs::{File, Metadata};
use std::os::unix::fs::MetadataExt;

use ofx_config::{ContextLimit, line_safe_prefix_length};
use ofx_text::{InvalidUtf8, Utf8Validator};
use ofx_workspace::PathError;
use tokio_util::sync::CancellationToken;

use super::SkillError;
use crate::io::read_positional_all;

const VALIDATION_CHUNK_BYTES: usize = 16 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;
const UTF8_LOOKAHEAD_BYTES: usize = 3;

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

pub(crate) fn read_skill_file(
    file: &File,
    limit: ContextLimit,
    safety_ceiling: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<SkillResourceRead, SkillError> {
    let before = metadata(file)?;
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
    if changed(&before, &metadata(file)?) {
        return Err(SkillError::SkillResourceChanged);
    }
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

fn metadata(file: &File) -> Result<Metadata, SkillError> {
    file.metadata()
        .map_err(|error| SkillError::Path(PathError::from(error)))
}

fn read_at(file: &File, buffer: &mut [u8], offset: usize) -> Result<usize, SkillError> {
    let offset = u64::try_from(offset).map_err(|_| SkillError::SkillFileLimitExceeded)?;
    read_positional_all(file, buffer, offset)
        .map_err(|error| SkillError::Path(PathError::from(error)))
}

fn changed(before: &Metadata, after: &Metadata) -> bool {
    before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::test_fixture::Fixture;

    #[test]
    fn skill_file_reads_notice_a_size_or_timestamp_change() {
        let fixture = Fixture::new();
        fixture.write("SKILL.md", "one\n");
        let path = fixture.path("SKILL.md");
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        let before = metadata(&file).unwrap();
        assert!(!changed(&before, &metadata(&file).unwrap()));

        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_nanos(1))
            .unwrap();
        let touched = metadata(&file).unwrap();
        assert!(changed(&before, &touched));

        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        assert!(changed(&touched, &metadata(&file).unwrap()));

        let resized = metadata(&file).unwrap();
        fs::write(&path, "one\ntwo\n").unwrap();
        file.set_modified(resized.modified().unwrap()).unwrap();
        assert!(changed(&resized, &metadata(&file).unwrap()));
    }
}
