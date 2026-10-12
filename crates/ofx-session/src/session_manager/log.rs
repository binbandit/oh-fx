use std::fs::File;
use std::os::unix::fs::FileExt;

use super::frame::{MAX_LINE_BYTES, check_frame};
use super::schema::{Header, parse_header};
use super::storage::{IoFault, StorageError};

const BLOCK_BYTES: usize = 64 << 10;
const TAIL_STEP_BYTES: usize = 4096;

pub(crate) fn check_line(line: &[u8]) -> Option<Header> {
    parse_header(check_frame(line)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadError {
    Corrupt(u64),
    Fault(IoFault),
}

impl From<std::io::Error> for ReadError {
    fn from(error: std::io::Error) -> Self {
        match StorageError::from(error) {
            StorageError::Fault(fault) => Self::Fault(fault),
            StorageError::NotFound => Self::Fault(IoFault::Io),
        }
    }
}

pub(crate) fn last_line_end(file: &File, len: u64) -> Result<u64, ReadError> {
    let mut buffer = [0_u8; TAIL_STEP_BYTES];
    let mut end = len;
    let mut scanned = 0_usize;
    while end > 0 {
        if scanned > MAX_LINE_BYTES + BLOCK_BYTES {
            return Err(ReadError::Corrupt(end));
        }
        let start = end.saturating_sub(TAIL_STEP_BYTES as u64);
        let count = usize::try_from(end - start).map_err(|_| ReadError::Fault(IoFault::Io))?;
        file.read_exact_at(&mut buffer[..count], start)?;
        if let Some(at) = buffer[..count].iter().rposition(|&byte| byte == b'\n') {
            return Ok(start + at as u64 + 1);
        }
        scanned += count;
        end = start;
    }
    Ok(0)
}

pub(crate) struct Line<'a> {
    pub(crate) offset: u64,
    pub(crate) header: Header,
    pub(crate) bytes: &'a [u8],
}

pub(crate) struct ForwardReader<'f> {
    file: &'f File,
    limit: u64,
    buffer: Vec<u8>,
    buffer_offset: u64,
    position: usize,
    expected_seq: Option<u64>,
}

impl<'f> ForwardReader<'f> {
    pub(crate) fn new(file: &'f File, offset: u64, limit: u64, expected_seq: Option<u64>) -> Self {
        Self {
            file,
            limit,
            buffer: Vec::new(),
            buffer_offset: offset,
            position: 0,
            expected_seq,
        }
    }

    pub(crate) fn next(&mut self) -> Result<Option<Line<'_>>, ReadError> {
        loop {
            if let Some(newline) = self.buffer[self.position..]
                .iter()
                .position(|&byte| byte == b'\n')
            {
                let start = self.position;
                let end = start + newline + 1;
                let offset = self.buffer_offset + start as u64;
                self.position = end;
                let Some(header) = check_line(&self.buffer[start..end]) else {
                    if offset + (end - start) as u64 >= self.limit {
                        return Ok(None);
                    }
                    return Err(ReadError::Corrupt(offset));
                };
                if self
                    .expected_seq
                    .is_some_and(|expected| header.seq != expected)
                {
                    return Err(ReadError::Corrupt(offset));
                }
                self.expected_seq = Some(header.seq + 1);
                return Ok(Some(Line {
                    offset,
                    header,
                    bytes: &self.buffer[start..end],
                }));
            }
            let have_end = self.buffer_offset + self.buffer.len() as u64;
            if have_end >= self.limit {
                return Ok(None);
            }
            if self.buffer.len() - self.position > MAX_LINE_BYTES {
                return Err(ReadError::Corrupt(
                    self.buffer_offset + self.position as u64,
                ));
            }
            self.buffer.drain(..self.position);
            self.buffer_offset += self.position as u64;
            self.position = 0;
            let want = usize::try_from((self.limit - have_end).min(BLOCK_BYTES as u64))
                .map_err(|_| ReadError::Fault(IoFault::Io))?;
            let old_len = self.buffer.len();
            self.buffer.resize(old_len + want, 0);
            let read = self.file.read_at(&mut self.buffer[old_len..], have_end)?;
            self.buffer.truncate(old_len + read);
            if read == 0 {
                return Ok(None);
            }
        }
    }
}

pub(crate) struct BackwardReader<'f> {
    file: &'f File,
    position: u64,
    buffer: Vec<u8>,
    buffer_offset: u64,
    expected_seq: Option<u64>,
}

impl<'f> BackwardReader<'f> {
    pub(crate) fn new(file: &'f File, end: u64) -> Self {
        Self {
            file,
            position: end,
            buffer: Vec::new(),
            buffer_offset: end,
            expected_seq: None,
        }
    }

    pub(crate) fn next(&mut self) -> Result<Option<Line<'_>>, ReadError> {
        if self.position == 0 {
            return Ok(None);
        }
        let held = usize::try_from(self.position - self.buffer_offset)
            .map_err(|_| ReadError::Fault(IoFault::Io))?;
        self.buffer.truncate(held);
        loop {
            let have = &self.buffer[..];
            if have.last().is_some_and(|&byte| byte != b'\n') {
                return Err(ReadError::Corrupt(self.position));
            }
            let search = &have[..have.len().saturating_sub(1)];
            let start = match search.iter().rposition(|&byte| byte == b'\n') {
                Some(newline) => Some(newline + 1),
                None if self.buffer_offset == 0 && !have.is_empty() => Some(0),
                None => None,
            };
            if let Some(start) = start {
                let header = check_line(&have[start..]).ok_or(ReadError::Corrupt(self.position))?;
                if self
                    .expected_seq
                    .is_some_and(|expected| header.seq != expected)
                {
                    return Err(ReadError::Corrupt(self.position));
                }
                self.expected_seq = Some(header.seq - 1);
                self.position = self.buffer_offset + start as u64;
                return Ok(Some(Line {
                    offset: self.position,
                    header,
                    bytes: &self.buffer[start..],
                }));
            }
            if have.len() > MAX_LINE_BYTES {
                return Err(ReadError::Corrupt(self.position));
            }
            let want = usize::try_from(self.buffer_offset.min(BLOCK_BYTES as u64))
                .map_err(|_| ReadError::Fault(IoFault::Io))?;
            let mut block = vec![0_u8; want];
            self.buffer_offset -= want as u64;
            self.file.read_exact_at(&mut block, self.buffer_offset)?;
            block.extend_from_slice(&self.buffer);
            self.buffer = block;
        }
    }
}

#[cfg(test)]
mod tests;
