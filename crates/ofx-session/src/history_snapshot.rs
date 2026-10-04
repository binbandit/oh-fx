use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::slice;

use ofx_config::PrivateDir;

use crate::session_event::{
    CONVERSATION_SCHEMA_VERSION, ConversationEnvelope, decode_history_envelope,
    encode_history_envelope,
};
use crate::session_log::managed_file::{Access, create_managed_file, open_managed_file};

pub(crate) const HISTORY_CACHE_FILE: &str = "history-cache.bin";
const MAGIC: &[u8; 18] = b"fx-history-cache\x1a\n";
const FORMAT_VERSION: u64 = 3;
const MAX_FRAME_BYTES: u32 = 1024 * 1024 * 1024;
const MAX_FRAMES: usize = 4 * 1024 * 1024;
const HEADER_BYTES: usize = MAGIC.len() + 24;
const FRAME_HEAD_BYTES: u64 = 8;
const PAYLOAD_PREFIX_BYTES: usize = 24;
const SEQ_END: usize = PAYLOAD_PREFIX_BYTES + 16;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const WRITE_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameMeta {
    file_offset: u64,
    log_offset: u64,
    log_bytes: u64,
    line_crc: u32,
}

impl FrameMeta {
    fn log_end(&self) -> u64 {
        self.log_offset + self.log_bytes
    }
}

pub(crate) struct HistoryCache {
    file: File,
    frames: Vec<FrameMeta>,
    tee: Tee,
}

struct Tee {
    flushed: u64,
    pending: Vec<u8>,
    covered: u64,
    room: usize,
    appended: Vec<FrameMeta>,
    broken: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct CacheView<'a> {
    file: &'a File,
    frames: &'a [FrameMeta],
}

pub(crate) struct CacheTee<'a> {
    file: &'a File,
    tee: &'a mut Tee,
}

pub(crate) struct CachedFrames<'a> {
    reader: BufReader<&'a File>,
    frames: slice::Iter<'a, FrameMeta>,
    payload: Vec<u8>,
}

pub(crate) struct CachedFrame {
    pub(crate) offset: u64,
    pub(crate) bytes: u64,
    pub(crate) envelope: ConversationEnvelope,
}

impl HistoryCache {
    pub(crate) fn open(
        dir: &PrivateDir,
        session_id: &str,
        log: &File,
        log_len: u64,
        access: Access,
    ) -> Option<Self> {
        let file = open_managed_file(dir, HISTORY_CACHE_FILE, access).ok()??;
        let (frames, len) = verify(&file, session_id, log, log_len)?;
        if access == Access::Writable && file.metadata().ok()?.len() != len {
            file.set_len(len).ok()?;
        }
        let covered = frames.last().map_or(0, FrameMeta::log_end);
        let tee = Tee::new(len, covered, MAX_FRAMES - frames.len());
        Some(Self { file, frames, tee })
    }

    pub(crate) fn create(dir: &PrivateDir, session_id: &str) -> Option<Self> {
        let _ = dir.remove(HISTORY_CACHE_FILE);
        let file = create_managed_file(dir, HISTORY_CACHE_FILE).ok()?;
        let mut tee = Tee::new(0, 0, MAX_FRAMES);
        tee.pending.extend_from_slice(MAGIC);
        tee.pending.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        tee.pending
            .extend_from_slice(&u64::from(CONVERSATION_SCHEMA_VERSION).to_le_bytes());
        let id_len = u32::try_from(session_id.len()).ok()?;
        tee.pending
            .extend_from_slice(&u64::from(id_len).to_le_bytes());
        tee.pending.extend_from_slice(session_id.as_bytes());
        Some(Self {
            file,
            frames: Vec::new(),
            tee,
        })
    }

    pub(crate) fn remove(dir: &PrivateDir) {
        let _ = dir.remove(HISTORY_CACHE_FILE);
    }

    #[cfg(test)]
    pub(crate) fn covered(&self) -> u64 {
        self.frames.last().map_or(0, FrameMeta::log_end)
    }

    pub(crate) fn view(&self) -> CacheView<'_> {
        CacheView {
            file: &self.file,
            frames: &self.frames,
        }
    }

    pub(crate) fn tee(&mut self) -> CacheTee<'_> {
        CacheTee {
            file: &self.file,
            tee: &mut self.tee,
        }
    }

    pub(crate) fn split(&mut self) -> (CacheView<'_>, CacheTee<'_>) {
        (
            CacheView {
                file: &self.file,
                frames: &self.frames,
            },
            CacheTee {
                file: &self.file,
                tee: &mut self.tee,
            },
        )
    }

    pub(crate) fn finish(mut self, dir: &PrivateDir, committed: u64) -> Option<Self> {
        if self.settle(committed).is_none() {
            drop(self);
            Self::remove(dir);
            return None;
        }
        Some(self)
    }

    fn settle(&mut self, committed: u64) -> Option<()> {
        self.tee().flush();
        if self.tee.broken {
            return None;
        }
        self.frames.append(&mut self.tee.appended);
        let kept = self
            .frames
            .partition_point(|frame| frame.log_end() <= committed);
        if let Some(first_dropped) = self.frames.get(kept) {
            self.file.set_len(first_dropped.file_offset).ok()?;
            self.frames.truncate(kept);
        }
        Some(())
    }
}

impl Tee {
    fn new(flushed: u64, covered: u64, room: usize) -> Self {
        Self {
            flushed,
            pending: Vec::new(),
            covered,
            room,
            appended: Vec::new(),
            broken: false,
        }
    }
}

impl CacheTee<'_> {
    pub(crate) fn append(&mut self, log_offset: u64, line: &[u8], envelope: &ConversationEnvelope) {
        if self.tee.broken || self.tee.appended.len() == self.tee.room {
            return;
        }
        if self.encode(log_offset, line, envelope).is_none() {
            self.tee.broken = true;
            return;
        }
        if self.tee.pending.len() >= WRITE_BUFFER_BYTES {
            self.flush();
        }
    }

    fn encode(
        &mut self,
        log_offset: u64,
        line: &[u8],
        envelope: &ConversationEnvelope,
    ) -> Option<()> {
        let tee = &mut *self.tee;
        if log_offset != tee.covered {
            return None;
        }
        let log_bytes = u64::from(u32::try_from(line.len()).ok()?);
        let line_crc = crc32fast::hash(line);
        let start = tee.pending.len();
        let file_offset = tee.flushed + u64::try_from(start).ok()?;
        tee.pending.extend_from_slice(&[0; 8]);
        tee.pending.extend_from_slice(&log_offset.to_le_bytes());
        tee.pending.extend_from_slice(&log_bytes.to_le_bytes());
        tee.pending
            .extend_from_slice(&u64::from(line_crc).to_le_bytes());
        encode_history_envelope(&mut tee.pending, envelope)?;
        let (head, payload) = tee.pending.get_mut(start..)?.split_at_mut(8);
        let frame_len = u32::try_from(payload.len()).ok()?.checked_add(4)?;
        if frame_len > MAX_FRAME_BYTES {
            return None;
        }
        let payload_crc = crc32fast::hash(payload);
        head[..4].copy_from_slice(&frame_len.to_le_bytes());
        head[4..].copy_from_slice(&payload_crc.to_le_bytes());
        tee.appended.push(FrameMeta {
            file_offset,
            log_offset,
            log_bytes,
            line_crc,
        });
        tee.covered += log_bytes;
        Some(())
    }

    fn flush(&mut self) {
        let tee = &mut *self.tee;
        if tee.broken || tee.pending.is_empty() {
            return;
        }
        if self.file.write_all_at(&tee.pending, tee.flushed).is_err() {
            tee.broken = true;
            return;
        }
        tee.flushed += u64::try_from(tee.pending.len()).unwrap_or(u64::MAX);
        tee.pending.clear();
    }
}

impl<'a> CacheView<'a> {
    pub(crate) fn frames(self, start: u64, end: u64) -> Option<CachedFrames<'a>> {
        let first = self
            .frames
            .partition_point(|frame| frame.log_offset < start);
        let stop = self.frames.partition_point(|frame| frame.log_end() <= end);
        let frames = self.frames.get(first..stop)?;
        let head = frames.first().filter(|frame| frame.log_offset == start)?;
        let mut file = self.file;
        file.seek(SeekFrom::Start(head.file_offset)).ok()?;
        Some(CachedFrames {
            reader: BufReader::with_capacity(READ_BUFFER_BYTES, file),
            frames: frames.iter(),
            payload: Vec::new(),
        })
    }
}

impl CachedFrames<'_> {
    fn read(&mut self, meta: FrameMeta) -> Option<CachedFrame> {
        let mut head = [0; 8];
        self.reader.read_exact(&mut head).ok()?;
        self.payload.resize(payload_len(head)?, 0);
        self.reader.read_exact(&mut self.payload).ok()?;
        let (prefix, envelope) = self.payload.split_at_checked(PAYLOAD_PREFIX_BYTES)?;
        if word(prefix, 0)? != meta.log_offset {
            return None;
        }
        Some(CachedFrame {
            offset: meta.log_offset,
            bytes: meta.log_bytes,
            envelope: decode_history_envelope(envelope)?,
        })
    }
}

impl Iterator for CachedFrames<'_> {
    type Item = CachedFrame;

    fn next(&mut self) -> Option<CachedFrame> {
        let meta = *self.frames.next()?;
        let frame = self.read(meta);
        if frame.is_none() {
            self.frames = [].iter();
        }
        frame
    }
}

fn payload_len(head: [u8; 8]) -> Option<usize> {
    let [a, b, c, d, ..] = head;
    let frame_len = u32::from_le_bytes([a, b, c, d]);
    if !(4..=MAX_FRAME_BYTES).contains(&frame_len) {
        return None;
    }
    usize::try_from(frame_len - 4).ok()
}

fn verify(
    file: &File,
    session_id: &str,
    log: &File,
    log_len: u64,
) -> Option<(Vec<FrameMeta>, u64)> {
    let file_len = file.metadata().ok()?.len();
    let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, file);
    let mut header = [0; HEADER_BYTES];
    reader.read_exact(&mut header).ok()?;
    let (magic, versions) = header.split_at(MAGIC.len());
    let id_len = usize::try_from(word(versions, 2)?).ok()?;
    let valid_header = magic == MAGIC
        && word(versions, 0)? == FORMAT_VERSION
        && word(versions, 1)? == u64::from(CONVERSATION_SCHEMA_VERSION)
        && id_len == session_id.len();
    if !valid_header {
        return None;
    }
    let mut id = vec![0; id_len];
    reader.read_exact(&mut id).ok()?;
    if id != session_id.as_bytes() {
        return None;
    }
    let mut file_offset = u64::try_from(HEADER_BYTES + id_len).ok()?;
    let mut frames: Vec<FrameMeta> = Vec::new();
    let mut covered = 0;
    while frames.len() < MAX_FRAMES {
        let expected_seq = u64::try_from(frames.len()).ok()? + 1;
        let next = next_frame(&mut reader, file_offset, file_len, covered, expected_seq);
        let Some((frame, size)) = next else {
            break;
        };
        covered += frame.log_bytes;
        file_offset += size;
        frames.push(frame);
    }
    let tail = frames.last()?;
    if covered > log_len {
        return None;
    }
    let mut line = vec![0; usize::try_from(tail.log_bytes).ok()?];
    log.read_exact_at(&mut line, tail.log_offset).ok()?;
    (crc32fast::hash(&line) == tail.line_crc).then_some((frames, file_offset))
}

fn next_frame(
    reader: &mut BufReader<&File>,
    file_offset: u64,
    file_len: u64,
    covered: u64,
    expected_seq: u64,
) -> Option<(FrameMeta, u64)> {
    let mut head = [0; 8];
    reader.read_exact(&mut head).ok()?;
    let payload_len = payload_len(head)?;
    let size = FRAME_HEAD_BYTES + u64::try_from(payload_len).ok()?;
    if file_offset + size > file_len {
        return None;
    }
    let [.., a, b, c, d] = head;
    let want_crc = u32::from_le_bytes([a, b, c, d]);
    let mut prefix = [0; SEQ_END];
    let crc = checksum(reader, size - FRAME_HEAD_BYTES, &mut prefix)?;
    if crc != want_crc || payload_len < SEQ_END {
        return None;
    }
    let log_offset = word(&prefix, 0)?;
    let log_bytes = u64::from(u32::try_from(word(&prefix, 1)?).ok()?);
    let line_crc = u32::try_from(word(&prefix, 2)?).ok()?;
    let seq = word(&prefix, 4)?;
    let frame = FrameMeta {
        file_offset,
        log_offset,
        log_bytes,
        line_crc,
    };
    (seq == expected_seq && log_offset == covered).then_some((frame, size))
}

fn word(bytes: &[u8], index: usize) -> Option<u64> {
    let start = index.checked_mul(8)?;
    Some(u64::from_le_bytes(
        bytes.get(start..start.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn checksum(reader: &mut impl BufRead, length: u64, prefix: &mut [u8]) -> Option<u32> {
    let mut hasher = crc32fast::Hasher::new();
    let mut remaining = length;
    let mut filled = 0;
    while remaining > 0 {
        let chunk = reader.fill_buf().ok()?;
        if chunk.is_empty() {
            return None;
        }
        let take = chunk
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let chunk = &chunk[..take];
        hasher.update(chunk);
        let copied = prefix.len().saturating_sub(filled).min(take);
        prefix[filled..filled + copied].copy_from_slice(&chunk[..copied]);
        filled += copied;
        reader.consume(take);
        remaining -= u64::try_from(take).ok()?;
    }
    Some(hasher.finalize())
}

#[cfg(test)]
mod tests;
