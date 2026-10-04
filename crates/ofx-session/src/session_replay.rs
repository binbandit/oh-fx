use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Take};

use crate::history_snapshot::{CacheView, CachedFrames};
use crate::session_error::SessionError;
use crate::session_event::{
    ConversationEnvelope, ConversationEvent, EVENT_FRAME_MAX_BYTES, decode_conversation_frame,
};

const READ_BUFFER_BYTES: usize = 8 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LineRead {
    Line(Vec<u8>),
    End,
    Torn,
}

pub(crate) struct LineReader<'a> {
    reader: BufReader<Take<&'a File>>,
    offset: u64,
}

impl<'a> LineReader<'a> {
    pub(crate) fn new(mut file: &'a File, start: u64, end: u64) -> Result<Self, SessionError> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Self {
            reader: BufReader::with_capacity(
                READ_BUFFER_BYTES,
                file.take(end.saturating_sub(start)),
            ),
            offset: start,
        })
    }

    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    pub(crate) fn next_line(&mut self) -> Result<LineRead, SessionError> {
        let mut line = Vec::new();
        let limit = u64::try_from(EVENT_FRAME_MAX_BYTES + 1).unwrap_or(u64::MAX);
        (&mut self.reader)
            .take(limit)
            .read_until(b'\n', &mut line)?;
        if line.len() > EVENT_FRAME_MAX_BYTES {
            return Err(SessionError::EventFrameTooLarge);
        }
        if line.is_empty() {
            return Ok(LineRead::End);
        }
        if line.last() != Some(&b'\n') {
            return Ok(LineRead::Torn);
        }
        self.offset += u64::try_from(line.len()).unwrap_or(u64::MAX);
        Ok(LineRead::Line(line))
    }
}

pub(crate) struct Frame {
    pub(crate) offset: u64,
    pub(crate) envelope: ConversationEnvelope,
    pub(crate) line: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
pub(crate) struct History<'a> {
    log: &'a File,
    cache: Option<CacheView<'a>>,
}

impl<'a> History<'a> {
    pub(crate) fn log(log: &'a File) -> Self {
        Self { log, cache: None }
    }

    pub(crate) fn cached(log: &'a File, cache: CacheView<'a>) -> Self {
        Self {
            log,
            cache: Some(cache),
        }
    }

    pub(crate) fn frames(self, start: u64, end: u64) -> FrameReader<'a> {
        FrameReader {
            cached: self.cache.and_then(|cache| cache.frames(start, end)),
            log: self.log,
            offset: start,
            end,
            lines: None,
            torn: false,
        }
    }

    pub(crate) fn event_at(self, offset: u64, end: u64) -> Result<ConversationEvent, SessionError> {
        self.frames(offset, end)
            .next_frame()?
            .map(|frame| frame.envelope.event)
            .ok_or(SessionError::InvalidConversationFrame)
    }
}

pub(crate) struct FrameReader<'a> {
    cached: Option<CachedFrames<'a>>,
    log: &'a File,
    offset: u64,
    end: u64,
    lines: Option<LineReader<'a>>,
    torn: bool,
}

impl FrameReader<'_> {
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    pub(crate) fn torn(&self) -> bool {
        self.torn
    }

    pub(crate) fn next_frame(&mut self) -> Result<Option<Frame>, SessionError> {
        if let Some(frame) = self.cached.as_mut().and_then(Iterator::next) {
            self.offset = frame.offset + frame.bytes;
            return Ok(Some(Frame {
                offset: frame.offset,
                envelope: frame.envelope,
                line: None,
            }));
        }
        self.cached = None;
        if self.lines.is_none() {
            self.lines = Some(LineReader::new(self.log, self.offset, self.end)?);
        }
        let Some(lines) = self.lines.as_mut() else {
            return Ok(None);
        };
        let line = match lines.next_line()? {
            LineRead::Line(line) => line,
            LineRead::End => return Ok(None),
            LineRead::Torn => {
                self.torn = true;
                return Ok(None);
            }
        };
        let envelope = decode_conversation_frame(&line)?;
        let frame = Frame {
            offset: self.offset,
            envelope,
            line: Some(line),
        };
        self.offset = lines.offset();
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn file_with(bytes: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    #[test]
    fn lines_keep_exact_boundaries_and_stop_at_the_given_end() {
        let file = file_with(b"one\ntwo\nthree\n");
        let mut reader = LineReader::new(&file, 4, 8).unwrap();
        assert_eq!(
            reader.next_line().unwrap(),
            LineRead::Line(b"two\n".to_vec())
        );
        assert_eq!(reader.offset(), 8);
        assert_eq!(reader.next_line().unwrap(), LineRead::End);
        let mut reader = LineReader::new(&file, 8, 12).unwrap();
        assert_eq!(reader.next_line().unwrap(), LineRead::Torn);
        assert_eq!(reader.offset(), 8);
    }

    #[test]
    fn a_final_line_without_its_newline_is_torn() {
        let file = file_with(b"{\"a\":1}\n{\"b\"");
        let mut reader = LineReader::new(&file, 0, 13).unwrap();
        assert!(matches!(reader.next_line().unwrap(), LineRead::Line(_)));
        assert_eq!(reader.next_line().unwrap(), LineRead::Torn);
        assert_eq!(reader.offset(), 8);
    }

    #[test]
    fn lines_longer_than_the_frame_cap_are_rejected() {
        let mut bytes = vec![b'x'; EVENT_FRAME_MAX_BYTES];
        bytes.push(b'\n');
        let file = file_with(&bytes);
        let end = u64::try_from(bytes.len()).unwrap();
        let mut reader = LineReader::new(&file, 0, end).unwrap();
        assert_eq!(reader.next_line(), Err(SessionError::EventFrameTooLarge));
        let mut reader = LineReader::new(&file, 1, end).unwrap();
        assert!(
            matches!(reader.next_line().unwrap(), LineRead::Line(line) if line.len() == EVENT_FRAME_MAX_BYTES)
        );
    }
}
