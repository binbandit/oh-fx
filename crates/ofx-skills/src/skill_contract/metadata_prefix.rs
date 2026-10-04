use std::fs::File;

use super::MAX_FRONTMATTER_BYTES;
use super::metadata::frontmatter_header_start;
use crate::io::read_positional_all;

const CHUNK_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataPrefixError {
    StreamTooLong,
    Unreadable,
    Operational(std::io::ErrorKind),
}

struct MetadataScanner {
    offset: usize,
    line_prefix: [u8; 4],
    line_len: usize,
}

impl MetadataScanner {
    fn new(header_start: usize) -> Self {
        Self {
            offset: header_start,
            line_prefix: [0; 4],
            line_len: 0,
        }
    }

    fn scan(&mut self, content: &[u8], complete: bool) -> bool {
        while let Some(&byte) = content.get(self.offset) {
            self.offset += 1;
            if byte == b'\n' {
                let closing = self.is_closing_line(true);
                self.line_len = 0;
                if closing {
                    return true;
                }
                continue;
            }
            if let Some(slot) = self.line_prefix.get_mut(self.line_len) {
                *slot = byte;
            }
            self.line_len += 1;
        }
        complete && self.is_closing_line(false)
    }

    fn is_closing_line(&self, newline_terminated: bool) -> bool {
        (self.line_len == 3 && self.line_prefix[..3] == *b"---")
            || (newline_terminated && self.line_len == 4 && self.line_prefix == *b"---\r")
    }
}

pub(crate) fn read_metadata_prefix(
    file: &File,
    file_size: usize,
) -> Result<Vec<u8>, MetadataPrefixError> {
    let readable_size = file_size.min(MAX_FRONTMATTER_BYTES + 1);
    let mut content = Vec::with_capacity(readable_size);
    let mut chunk = [0; CHUNK_BYTES];
    let mut scanner: Option<MetadataScanner> = None;
    let mut metadata_complete = false;

    while content.len() < readable_size {
        let wanted = chunk.len().min(readable_size - content.len());
        let offset = u64::try_from(content.len()).map_err(|_| MetadataPrefixError::Unreadable)?;
        let read = read_positional_all(file, &mut chunk[..wanted], offset)
            .map_err(|error| MetadataPrefixError::Operational(error.kind()))?;
        if read == 0 {
            return Err(MetadataPrefixError::Unreadable);
        }
        content.extend_from_slice(&chunk[..read]);

        let file_complete = content.len() == file_size;
        if scanner.is_none() {
            if content.len() < 5 && !file_complete {
                continue;
            }
            let Some(header_start) = frontmatter_header_start(&content) else {
                metadata_complete = true;
                break;
            };
            scanner = Some(MetadataScanner::new(header_start));
        }
        if scanner
            .as_mut()
            .is_some_and(|active| active.scan(&content, file_complete))
        {
            metadata_complete = true;
            break;
        }
    }
    if !metadata_complete && content.len() < file_size {
        return Err(MetadataPrefixError::StreamTooLong);
    }
    Ok(content)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_scanner_examines_malformed_and_late_delimiters_once() {
        let mut malformed = vec![b'a'; 256 * 1024 + 37];
        let malformed_header = b"---\nname: linear\n";
        malformed[..malformed_header.len()].copy_from_slice(malformed_header);
        let mut malformed_scanner = MetadataScanner::new(4);
        let mut malformed_end = 0;
        let mut malformed_found = false;
        while malformed_end < malformed.len() {
            malformed_end = (malformed_end + CHUNK_BYTES).min(malformed.len());
            malformed_found = malformed_scanner.scan(
                &malformed[..malformed_end],
                malformed_end == malformed.len(),
            );
        }
        assert!(!malformed_found);
        assert_eq!(malformed_scanner.offset, malformed.len());

        let mut late = vec![b'b'; 256 * 1024 + 37];
        let late_header = b"---\nname: late\n";
        late[..late_header.len()].copy_from_slice(late_header);
        let late_len = late.len();
        late[late_len - 4..].copy_from_slice(b"\n---");
        let mut late_scanner = MetadataScanner::new(4);
        let mut late_end = 0;
        let mut late_found = false;
        while late_end < late.len() && !late_found {
            late_end = (late_end + CHUNK_BYTES).min(late.len());
            late_found = late_scanner.scan(&late[..late_end], late_end == late.len());
        }
        assert!(late_found);
        assert_eq!(late_scanner.offset, late.len());
    }
    #[test]
    fn metadata_read_retains_operational_error_classification() {
        use std::os::unix::fs::FileExt;
        let fixture = crate::test_fixture::Fixture::new();
        fixture.write("SKILL.md", "---\nname: review\n---\n");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(fixture.path("SKILL.md"))
            .unwrap();
        let kind = file.read_at(&mut [0], 0).unwrap_err().kind();
        let result = read_metadata_prefix(
            &file,
            usize::try_from(file.metadata().unwrap().len()).unwrap(),
        );
        assert_eq!(
            format!("{:?}", result.unwrap_err()),
            format!("Operational({kind:?})")
        );
    }
}
