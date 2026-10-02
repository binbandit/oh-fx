use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir};
use rustix::fs::{self, FileType, Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::session_log::managed_file::{file_type, permissions, private_file_mode};
use crate::session_store_paths::MAX_PATH_BYTES;

const HISTORY_FILE: &str = "history.jsonl";
const HISTORY_LOCK_FILE: &str = "history.lock";
const LOCK_DEADLINE: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(10);
const DEFAULT_SCAN_BLOCK_BYTES: usize = 1024 * 1024;
const MAX_RECORD_BYTES: usize = 256 * 1024;
const COMPACTION_THRESHOLD_BYTES: u64 = 1024 * 1024;
const COMPACTION_RECORD_LIMIT: usize = 1000;
const COMPACTION_BYTE_LIMIT: usize = 1024 * 1024;
const LINE_CHUNK_BYTES: usize = 8192;
const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended,
    Duplicate,
    RecordTooLarge,
    CompactionStale,
    CompactionIndeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PromptHistoryError {
    #[error("InvalidDurableField")]
    InvalidDurableField,
    #[error("PromptHistoryLockBusy")]
    LockBusy,
    #[error("PromptHistoryLockUnsupported")]
    LockUnsupported,
    #[error("PromptHistoryWriteFailed")]
    WriteFailed,
    #[error("DurableLayoutFailed")]
    LayoutFailed,
    #[error("DurablePathUnsafe")]
    PathUnsafe,
    #[error("PrivateStatePermissionsUnsupported")]
    PermissionsUnsupported,
    #[error("{0:?}")]
    Io(io::ErrorKind),
}

impl From<io::Error> for PromptHistoryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

impl From<Errno> for PromptHistoryError {
    fn from(errno: Errno) -> Self {
        io::Error::from(errno).into()
    }
}

impl From<DurableError> for PromptHistoryError {
    fn from(error: DurableError) -> Self {
        match error {
            DurableError::PathUnsafe => Self::PathUnsafe,
            DurableError::PermissionsUnsupported => Self::PermissionsUnsupported,
            DurableError::LockUnsupported => Self::LockUnsupported,
            _ => Self::LayoutFailed,
        }
    }
}

#[derive(Serialize)]
struct RecordWire<'a> {
    schema_version: i64,
    timestamp_ms: i64,
    workspace_root: &'a str,
    text: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedRecord {
    schema_version: i64,
    timestamp_ms: i64,
    workspace_root: String,
    text: String,
}

enum CompactionFailure {
    Stale,
    Indeterminate,
}

pub struct PromptHistoryStore {
    data_dir: PathBuf,
    home: Option<PrivateDir>,
    indeterminate: bool,
    scan_block_bytes: usize,
    lock_deadline: Duration,
}

impl PromptHistoryStore {
    pub fn open(data_dir: &Path) -> Result<Self, PromptHistoryError> {
        let home = PrivateDir::open_existing(data_dir)?;
        Ok(Self {
            data_dir: data_dir.to_owned(),
            home,
            indeterminate: false,
            scan_block_bytes: DEFAULT_SCAN_BLOCK_BYTES,
            lock_deadline: LOCK_DEADLINE,
        })
    }

    pub fn append(
        &mut self,
        timestamp_ms: i64,
        workspace_root: &str,
        text: &str,
    ) -> Result<AppendOutcome, PromptHistoryError> {
        validate_workspace_root(workspace_root)?;
        let line = serialize_record(timestamp_ms, workspace_root, text)?;
        if line.len() > MAX_RECORD_BYTES {
            return Ok(AppendOutcome::RecordTooLarge);
        }
        self.ensure_writable()?;
        let _lock = self.acquire_lock()?;
        self.resolve_indeterminate()?;
        let file = self
            .open_history(true, true)?
            .ok_or(PromptHistoryError::WriteFailed)?;
        repair_incomplete_tail(&file)?;
        let boundary = file.metadata()?.len();
        let latest = self.load_recent_from_file(&file, workspace_root, 1, boundary)?;
        if latest.first().is_some_and(|latest| latest == text) {
            return Ok(AppendOutcome::Duplicate);
        }
        file.write_all_at(&line, boundary)?;
        file.sync_all()
            .map_err(|_| PromptHistoryError::WriteFailed)?;
        let committed = boundary + line.len() as u64;
        if committed <= COMPACTION_THRESHOLD_BYTES {
            return Ok(AppendOutcome::Appended);
        }
        Ok(match self.compact() {
            Ok(()) => AppendOutcome::Appended,
            Err(CompactionFailure::Stale) => AppendOutcome::CompactionStale,
            Err(CompactionFailure::Indeterminate) => AppendOutcome::CompactionIndeterminate,
        })
    }

    pub fn load_recent(
        &mut self,
        workspace_root: &str,
        limit: usize,
    ) -> Result<Vec<String>, PromptHistoryError> {
        validate_workspace_root(workspace_root)?;
        self.resolve_indeterminate()?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(file) = self.open_history(false, false)? else {
            return Ok(Vec::new());
        };
        let boundary = file.metadata()?.len();
        self.load_recent_from_file(&file, workspace_root, limit, boundary)
    }

    fn ensure_writable(&mut self) -> Result<(), PromptHistoryError> {
        let home = match self.home.take() {
            Some(home) => home,
            None => PrivateDir::open_or_create(&self.data_dir)?,
        };
        let home = self.home.insert(home);
        home.ensure_private()?;
        Ok(())
    }

    fn acquire_lock(&self) -> Result<AdvisoryLock, PromptHistoryError> {
        let home = self.home.as_ref().ok_or(PromptHistoryError::LayoutFailed)?;
        let started = Instant::now();
        loop {
            if let Some(lock) = home.try_lock(HISTORY_LOCK_FILE)? {
                return Ok(lock);
            }
            if started.elapsed() >= self.lock_deadline {
                return Err(PromptHistoryError::LockBusy);
            }
            thread::sleep(LOCK_RETRY.min(self.lock_deadline));
        }
    }

    fn open_history(
        &self,
        writable: bool,
        create: bool,
    ) -> Result<Option<File>, PromptHistoryError> {
        let Some(home) = &self.home else {
            return Ok(None);
        };
        let access = if writable {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        };
        let flags = access | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let mut created = false;
        let fd = match fs::openat(home, HISTORY_FILE, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) if !create => return Ok(None),
            Err(Errno::NOENT) => {
                match fs::openat(
                    home,
                    HISTORY_FILE,
                    OFlags::RDWR
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC
                        | OFlags::CREATE
                        | OFlags::EXCL,
                    private_file_mode(),
                ) {
                    Ok(fd) => {
                        created = true;
                        fd
                    }
                    Err(Errno::EXIST) => return self.open_history(writable, false),
                    Err(errno) => return Err(errno.into()),
                }
            }
            Err(Errno::LOOP | Errno::ISDIR | Errno::NOTDIR | Errno::NXIO) => {
                return Err(PromptHistoryError::PathUnsafe);
            }
            Err(errno) => return Err(errno.into()),
        };
        let stat = fs::fstat(&fd)?;
        if file_type(&stat) != FileType::RegularFile || stat.st_nlink != 1 {
            return Err(PromptHistoryError::PathUnsafe);
        }
        let stat = if writable {
            fs::fchmod(&fd, private_file_mode())
                .map_err(|_| PromptHistoryError::PermissionsUnsupported)?;
            fs::fstat(&fd)?
        } else {
            stat
        };
        if permissions(&stat) != private_file_mode() {
            return Err(PromptHistoryError::PermissionsUnsupported);
        }
        if created {
            fs::fsync(home).map_err(|_| PromptHistoryError::LayoutFailed)?;
        }
        Ok(Some(File::from(fd)))
    }

    fn resolve_indeterminate(&mut self) -> Result<(), PromptHistoryError> {
        if !self.indeterminate {
            return Ok(());
        }
        let file = self
            .open_history(true, false)?
            .ok_or(PromptHistoryError::WriteFailed)?;
        let length = file.metadata()?.len();
        if length > 0 && read_byte_at(&file, length - 1)? != Some(b'\n') {
            return Err(PromptHistoryError::WriteFailed);
        }
        self.indeterminate = false;
        Ok(())
    }

    fn compact(&mut self) -> Result<(), CompactionFailure> {
        let Ok(Some(file)) = self.open_history(false, false) else {
            return Err(CompactionFailure::Stale);
        };
        let replacement = file
            .metadata()
            .map_err(PromptHistoryError::from)
            .and_then(|metadata| compact_records(&file, metadata.len()))
            .map_err(|_| CompactionFailure::Stale)?;
        let home = self.home.as_ref().ok_or(CompactionFailure::Stale)?;
        match home.replace(HISTORY_FILE, &replacement) {
            Ok(()) => Ok(()),
            Err(DurableError::PostRenameFailed) => {
                self.indeterminate = true;
                Err(CompactionFailure::Indeterminate)
            }
            Err(_) => Err(CompactionFailure::Stale),
        }
    }

    fn load_recent_from_file(
        &self,
        file: &File,
        workspace_root: &str,
        limit: usize,
        requested_boundary: u64,
    ) -> Result<Vec<String>, PromptHistoryError> {
        let boundary = file.metadata()?.len().min(requested_boundary);
        if boundary == 0 || limit == 0 {
            return Ok(Vec::new());
        }
        let mut scan = ReverseScan {
            workspace_root,
            limit,
            entries: Vec::new(),
            pending: Vec::new(),
            pending_oversized: false,
            ignore_incomplete_tail: read_byte_at(file, boundary - 1)? != Some(b'\n'),
        };
        let mut cursor = boundary;
        let mut block = Vec::new();
        while cursor > 0 && !scan.full() {
            let block_len = usize::try_from(cursor).map_or(self.scan_block_bytes, |cursor| {
                cursor.min(self.scan_block_bytes)
            });
            let start = cursor - block_len as u64;
            block.resize(block_len, 0);
            read_exact_at(file, &mut block, start)?;
            scan.block(&block);
            cursor = start;
        }
        if cursor == 0 {
            scan.finish();
        }
        let mut entries = scan.entries;
        entries.reverse();
        Ok(entries)
    }
}

struct ReverseScan<'a> {
    workspace_root: &'a str,
    limit: usize,
    entries: Vec<String>,
    pending: Vec<u8>,
    pending_oversized: bool,
    ignore_incomplete_tail: bool,
}

impl ReverseScan<'_> {
    fn full(&self) -> bool {
        self.entries.len() >= self.limit
    }

    fn block(&mut self, block: &[u8]) {
        let mut segment_end = block.len();
        let mut index = block.len();
        while index > 0 && !self.full() {
            index -= 1;
            if block[index] != b'\n' {
                continue;
            }
            if self.ignore_incomplete_tail {
                self.ignore_incomplete_tail = false;
            } else {
                self.reverse_line(&block[index + 1..segment_end]);
            }
            self.pending.clear();
            self.pending_oversized = false;
            segment_end = index;
        }
        if !self.ignore_incomplete_tail && segment_end > 0 {
            if self.pending_oversized || segment_end + self.pending.len() + 1 > MAX_RECORD_BYTES {
                self.pending.clear();
                self.pending_oversized = true;
            } else {
                let mut joined = block[..segment_end].to_vec();
                joined.extend_from_slice(&self.pending);
                self.pending = joined;
            }
        }
    }

    fn reverse_line(&mut self, piece: &[u8]) {
        if self.full()
            || self.pending_oversized
            || piece.len() + self.pending.len() + 1 > MAX_RECORD_BYTES
        {
            return;
        }
        if self.pending.is_empty() {
            if !piece.is_empty() {
                self.complete_line(piece);
            }
            return;
        }
        let mut line = Vec::with_capacity(piece.len() + self.pending.len());
        line.extend_from_slice(piece);
        line.extend_from_slice(&self.pending);
        self.complete_line(&line);
    }

    fn finish(&mut self) {
        if !self.ignore_incomplete_tail
            && !self.pending_oversized
            && !self.pending.is_empty()
            && !self.full()
        {
            let pending = std::mem::take(&mut self.pending);
            self.complete_line(&pending);
        }
    }

    fn complete_line(&mut self, line: &[u8]) {
        if line.len() + 1 > MAX_RECORD_BYTES {
            return;
        }
        if let Some(record) = parse_record(line)
            && record.workspace_root == self.workspace_root
        {
            self.entries.push(record.text);
        }
    }
}

fn validate_workspace_root(workspace_root: &str) -> Result<(), PromptHistoryError> {
    if workspace_root.is_empty()
        || workspace_root.len() > MAX_PATH_BYTES
        || !workspace_root.starts_with('/')
    {
        return Err(PromptHistoryError::InvalidDurableField);
    }
    Ok(())
}

fn serialize_record(
    timestamp_ms: i64,
    workspace_root: &str,
    text: &str,
) -> Result<Vec<u8>, PromptHistoryError> {
    let mut line = serde_json::to_vec(&RecordWire {
        schema_version: SCHEMA_VERSION,
        timestamp_ms,
        workspace_root,
        text,
    })
    .map_err(|_| PromptHistoryError::InvalidDurableField)?;
    line.push(b'\n');
    Ok(line)
}

fn parse_record(line: &[u8]) -> Option<ParsedRecord> {
    let record: ParsedRecord = serde_json::from_slice(line).ok()?;
    (record.schema_version == SCHEMA_VERSION
        && validate_workspace_root(&record.workspace_root).is_ok())
    .then_some(record)
}

fn repair_incomplete_tail(file: &File) -> Result<(), PromptHistoryError> {
    let length = file.metadata()?.len();
    if length == 0 || read_byte_at(file, length - 1)? == Some(b'\n') {
        return Ok(());
    }
    let mut cursor = length;
    let mut buffer = [0_u8; LINE_CHUNK_BYTES];
    while cursor > 0 {
        let start = cursor - cursor.min(LINE_CHUNK_BYTES as u64);
        let chunk = &mut buffer[..usize::try_from(cursor - start).unwrap_or(0)];
        read_exact_at(file, chunk, start)?;
        if let Some(newline) = chunk.iter().rposition(|byte| *byte == b'\n') {
            file.set_len(start + newline as u64 + 1)?;
            file.sync_all()?;
            return Ok(());
        }
        cursor = start;
    }
    file.set_len(0)?;
    file.sync_all()?;
    Ok(())
}

fn compact_records(file: &File, length: u64) -> Result<Vec<u8>, PromptHistoryError> {
    let mut retained: VecDeque<Vec<u8>> = VecDeque::new();
    let mut retained_bytes = 0;
    let mut lines = LineReader {
        file,
        offset: 0,
        end: length,
    };
    while let Some(line) = lines.next_line()? {
        let Some(body) = line.strip_suffix(b"\n") else {
            continue;
        };
        if line.len() > MAX_RECORD_BYTES {
            continue;
        }
        let Some(record) = parse_record(body) else {
            continue;
        };
        let canonical =
            serialize_record(record.timestamp_ms, &record.workspace_root, &record.text)?;
        if canonical.len() > MAX_RECORD_BYTES {
            continue;
        }
        retained_bytes += canonical.len();
        retained.push_back(canonical);
        while retained.len() > COMPACTION_RECORD_LIMIT || retained_bytes > COMPACTION_BYTE_LIMIT {
            if let Some(oldest) = retained.pop_front() {
                retained_bytes -= oldest.len();
            }
        }
    }
    Ok(retained.into_iter().flatten().collect())
}

struct LineReader<'a> {
    file: &'a File,
    offset: u64,
    end: u64,
}

impl LineReader<'_> {
    fn next_line(&mut self) -> Result<Option<Vec<u8>>, PromptHistoryError> {
        let mut line = Vec::new();
        let mut chunk = [0_u8; LINE_CHUNK_BYTES];
        while self.offset < self.end {
            let count = self.read_chunk(&mut chunk)?;
            if count == 0 {
                return Ok(None);
            }
            if let Some(newline) = chunk[..count].iter().position(|byte| *byte == b'\n') {
                line.extend_from_slice(&chunk[..=newline]);
                self.offset += newline as u64 + 1;
                return Ok(Some(line));
            }
            line.extend_from_slice(&chunk[..count]);
            self.offset += count as u64;
            if line.len() > MAX_RECORD_BYTES {
                self.skip_past_newline(&mut chunk)?;
                return Ok(Some(line));
            }
        }
        Ok(None)
    }

    fn skip_past_newline(&mut self, chunk: &mut [u8]) -> Result<(), PromptHistoryError> {
        while self.offset < self.end {
            let count = self.read_chunk(chunk)?;
            if count == 0 {
                self.offset = self.end;
                return Ok(());
            }
            if let Some(newline) = chunk[..count].iter().position(|byte| *byte == b'\n') {
                self.offset += newline as u64 + 1;
                return Ok(());
            }
            self.offset += count as u64;
        }
        Ok(())
    }

    fn read_chunk(&self, chunk: &mut [u8]) -> Result<usize, PromptHistoryError> {
        let wanted = usize::try_from(self.end - self.offset)
            .unwrap_or(usize::MAX)
            .min(chunk.len());
        Ok(self.file.read_at(&mut chunk[..wanted], self.offset)?)
    }
}

fn read_byte_at(file: &File, offset: u64) -> Result<Option<u8>, PromptHistoryError> {
    let mut byte = [0_u8; 1];
    Ok((file.read_at(&mut byte, offset)? == 1).then_some(byte[0]))
}

fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<(), PromptHistoryError> {
    file.read_exact_at(buffer, offset).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            PromptHistoryError::WriteFailed
        } else {
            error.into()
        }
    })
}

#[cfg(test)]
mod tests;
