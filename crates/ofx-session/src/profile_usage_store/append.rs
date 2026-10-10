use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir};
use ofx_contract::{UsageCompleteness, UsageIncident};
use rustix::fs::{self, FileType, Mode, OFlags};
use rustix::io::Errno;

use super::records::{AppendOutcome, ProfileEvent, push_coverage, push_incident};
use super::{
    LOCK_RETRY, MAX_FILE_BYTES, MAX_RECORD_BYTES, MAX_RECORDS, ProfileUsageStore, RecordIndex,
    USAGE_FILE, USAGE_LOCK_FILE, UsageStoreError, validate_readable,
};
use crate::session_log::managed_file::{file_type, permissions, private_file_mode};
use crate::session_log::now_ms;

const TAIL_SAMPLE_BYTES: usize = 32;
const TAIL_SCAN_BYTES: usize = 8192;

struct Tail {
    length: u64,
    incomplete: bool,
}

impl ProfileUsageStore {
    pub(crate) fn append_event(
        &mut self,
        event: ProfileEvent<'_>,
    ) -> Result<AppendOutcome, UsageStoreError> {
        event.validate()?;
        let line = event.line();
        if line.len() > MAX_RECORD_BYTES {
            return Err(UsageStoreError::RecordTooLarge);
        }
        let home = writable_home(&mut self.home, &self.data_dir)?;
        let _lock = acquire_lock(home, self.lock_deadline, &self.abandoned)?;
        let file = open_writable_ledger(home)?;
        let tail = inspect_tail(&file)?;
        let index = ensure_index(&mut self.index, &file, tail.length, &self.abandoned)?;
        let decision = index.classify(event);
        let now = now_ms().max(0);
        let mut appended = String::new();
        let mut records = 0;
        if index.loaded.coverage_started_at_ms.is_none() {
            push_coverage(&mut appended, now);
            records += 1;
        }
        if tail.incomplete {
            push_incident(
                &mut appended,
                &UsageIncident {
                    occurred_at_ms: now,
                    completeness: UsageCompleteness::Incomplete,
                },
            );
            records += 1;
        }
        if decision.write {
            appended.push_str(&line);
            records += 1;
        }
        if appended.is_empty() {
            return Ok(decision.outcome);
        }
        let next_length = u64::try_from(appended.len())
            .ok()
            .and_then(|added| tail.length.checked_add(added))
            .filter(|length| *length <= MAX_FILE_BYTES)
            .ok_or(UsageStoreError::CapacityExceeded)?;
        if index.record_count.saturating_add(records) > MAX_RECORDS {
            return Err(UsageStoreError::CapacityExceeded);
        }
        if tail.incomplete {
            let mut replacement = read_prefix(&file, tail.length)?;
            replacement.extend_from_slice(appended.as_bytes());
            home.replace(USAGE_FILE, &replacement)
                .map_err(|error| match error {
                    DurableError::PostRenameFailed => UsageStoreError::CommitIndeterminate,
                    DurableError::PreRenameFailed => UsageStoreError::WriteFailed,
                    error => error.into(),
                })?;
        } else {
            file.write_all_at(appended.as_bytes(), tail.length)
                .and_then(|()| file.sync_all())
                .map_err(|_| UsageStoreError::WriteFailed)?;
        }
        if index.absorb_bytes(appended.as_bytes(), None).is_ok() {
            index.boundary = next_length;
            index.sample_tail(appended.as_bytes());
        } else {
            self.index = None;
        }
        Ok(decision.outcome)
    }
}

fn writable_home<'a>(
    home: &'a mut Option<PrivateDir>,
    data_dir: &std::path::Path,
) -> Result<&'a PrivateDir, UsageStoreError> {
    if home.is_none() {
        *home = Some(
            PrivateDir::open_or_create(data_dir).map_err(|error| match error {
                DurableError::PathUnsafe | DurableError::PermissionsUnsupported => error.into(),
                _ => UsageStoreError::LayoutFailed,
            })?,
        );
    }
    let home: &'a Option<PrivateDir> = home;
    let home = home.as_ref().ok_or(UsageStoreError::LayoutFailed)?;
    home.ensure_private()
        .map_err(|_| UsageStoreError::PermissionsUnsupported)?;
    validate_readable(home)?;
    Ok(home)
}

fn acquire_lock(
    home: &PrivateDir,
    deadline: Duration,
    abandoned: &AtomicBool,
) -> Result<AdvisoryLock, UsageStoreError> {
    let started = Instant::now();
    loop {
        if abandoned.load(Ordering::Acquire) {
            return Err(UsageStoreError::LockAbandoned);
        }
        match home.try_lock(USAGE_LOCK_FILE) {
            Ok(Some(lock)) => return Ok(lock),
            Ok(None) => {}
            Err(DurableError::LockUnsupported) => return Err(UsageStoreError::LockUnsupported),
            Err(error) => return Err(error.into()),
        }
        if started.elapsed() >= deadline {
            return Err(UsageStoreError::LockBusy);
        }
        thread::sleep(LOCK_RETRY.min(deadline));
    }
}

fn open_writable_ledger(home: &PrivateDir) -> Result<File, UsageStoreError> {
    let flags =
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let (file, created) = match fs::openat(home.as_fd(), USAGE_FILE, flags, Mode::empty()) {
        Ok(file) => (file, false),
        Err(Errno::NOENT) => match fs::openat(
            home.as_fd(),
            USAGE_FILE,
            flags | OFlags::CREATE | OFlags::EXCL,
            private_file_mode(),
        ) {
            Ok(file) => (file, true),
            Err(Errno::EXIST) => return open_writable_ledger(home),
            Err(errno) => return Err(errno.into()),
        },
        Err(Errno::LOOP | Errno::ISDIR | Errno::NOTDIR | Errno::NXIO) => {
            return Err(UsageStoreError::PathUnsafe);
        }
        Err(errno) => return Err(errno.into()),
    };
    let stat = fs::fstat(&file)?;
    if file_type(&stat) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(UsageStoreError::PathUnsafe);
    }
    fs::fchmod(&file, private_file_mode()).map_err(|_| UsageStoreError::PermissionsUnsupported)?;
    if permissions(&fs::fstat(&file)?) != private_file_mode() {
        return Err(UsageStoreError::PermissionsUnsupported);
    }
    if created {
        fs::fsync(home.as_fd()).map_err(|_| UsageStoreError::LayoutFailed)?;
    }
    Ok(File::from(file))
}

fn inspect_tail(file: &File) -> Result<Tail, UsageStoreError> {
    let length = file
        .metadata()
        .map_err(|_| UsageStoreError::ReadFailed)?
        .len();
    if length == 0 {
        return Ok(Tail {
            length,
            incomplete: false,
        });
    }
    let mut last = [0];
    if file.read_exact_at(&mut last, length - 1).is_ok() && last == *b"\n" {
        return Ok(Tail {
            length,
            incomplete: false,
        });
    }
    let mut cursor = length;
    let mut buffer = [0; TAIL_SCAN_BYTES];
    while cursor > 0 {
        let start = cursor.saturating_sub(TAIL_SCAN_BYTES as u64);
        let chunk = &mut buffer[..usize::try_from(cursor - start).unwrap_or(TAIL_SCAN_BYTES)];
        file.read_exact_at(chunk, start)
            .map_err(|_| UsageStoreError::WriteFailed)?;
        if let Some(newline) = chunk.iter().rposition(|byte| *byte == b'\n') {
            return Ok(Tail {
                length: start + newline as u64 + 1,
                incomplete: true,
            });
        }
        cursor = start;
    }
    Ok(Tail {
        length: 0,
        incomplete: true,
    })
}

fn read_prefix(file: &File, length: u64) -> Result<Vec<u8>, UsageStoreError> {
    let length = usize::try_from(length).map_err(|_| UsageStoreError::CapacityExceeded)?;
    let mut bytes = vec![0; length];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|_| UsageStoreError::ReadFailed)?;
    Ok(bytes)
}

fn ensure_index<'a>(
    slot: &'a mut Option<RecordIndex>,
    file: &File,
    boundary: u64,
    abandoned: &AtomicBool,
) -> Result<&'a mut RecordIndex, UsageStoreError> {
    let current = slot.as_mut().is_some_and(|index| {
        (boundary == index.boundary && index.tail_matches(file))
            || (boundary > index.boundary && index.absorb_tail(file, boundary, abandoned))
    });
    if !current {
        *slot = None;
        if boundary > MAX_FILE_BYTES {
            return Err(UsageStoreError::CapacityExceeded);
        }
        let mut fresh = RecordIndex::default();
        if boundary > 0 {
            let bytes = read_prefix(file, boundary)?;
            if bytes.last() != Some(&b'\n') {
                return Err(UsageStoreError::Incomplete);
            }
            fresh.absorb_bytes(&bytes, Some(abandoned))?;
            fresh.boundary = boundary;
            fresh.sample_tail(&bytes);
        }
        *slot = Some(fresh);
    }
    slot.as_mut().ok_or(UsageStoreError::Invalid)
}

impl RecordIndex {
    fn sample_tail(&mut self, bytes: &[u8]) {
        let start = bytes.len().saturating_sub(TAIL_SAMPLE_BYTES);
        self.tail_sample = bytes[start..].to_vec();
    }

    fn tail_matches(&self, file: &File) -> bool {
        let Ok(length) = u64::try_from(self.tail_sample.len()) else {
            return false;
        };
        if length == 0 || length > self.boundary {
            return false;
        }
        let mut sample = vec![0; self.tail_sample.len()];
        file.read_exact_at(&mut sample, self.boundary - length)
            .is_ok_and(|()| sample == self.tail_sample)
    }

    fn absorb_tail(&mut self, file: &File, boundary: u64, abandoned: &AtomicBool) -> bool {
        if boundary - self.boundary > MAX_FILE_BYTES || !self.tail_matches(file) {
            return false;
        }
        let Ok(length) = usize::try_from(boundary - self.boundary) else {
            return false;
        };
        let mut bytes = vec![0; length];
        if file.read_exact_at(&mut bytes, self.boundary).is_err()
            || bytes.last() != Some(&b'\n')
            || self.absorb_bytes(&bytes, Some(abandoned)).is_err()
        {
            return false;
        }
        self.boundary = boundary;
        self.sample_tail(&bytes);
        true
    }
}
