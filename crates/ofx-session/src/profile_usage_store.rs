use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ofx_config::{DurableError, PrivateDir};
use ofx_contract::{GenerationFact, PendingMarker, UsageCompleteness, UsageIncident};
use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::generation_fact_codec::{self, non_negative};
use crate::json_fields::parse_json;
use crate::session_log::managed_file::{file_type, permissions, private_file_mode};

const USAGE_FILE: &str = "usage.jsonl";
const USAGE_LOCK_FILE: &str = "usage.lock";
const LOCK_DEADLINE: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(10);
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_RECORDS: usize = 200_000;
const SCHEMA_VERSION: u64 = 1;
const COVERAGE_FIELDS: usize = 3;
const GENERATION_FIELDS: usize = 3;
const PENDING_FIELDS: usize = 4;
const INCIDENT_FIELDS: usize = 4;
const ABANDON_CHECK_LINES: usize = 256;

mod append;
mod compaction;
mod records;

pub(crate) use records::{AppendOutcome, ProfileEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UsageStoreError {
    #[error("DurablePathUnsafe")]
    PathUnsafe,
    #[error("PrivateStatePermissionsUnsupported")]
    PermissionsUnsupported,
    #[error("UsageLockBusy")]
    LockBusy,
    #[error("UsageLockUnsupported")]
    LockUnsupported,
    #[error("UsageStoreIncomplete")]
    Incomplete,
    #[error("InvalidUsageStore")]
    Invalid,
    #[error("UsageCapacityExceeded")]
    CapacityExceeded,
    #[error("UsageReadFailed")]
    ReadFailed,
    #[error("UsageWriteFailed")]
    WriteFailed,
    #[error("UsageCommitIndeterminate")]
    CommitIndeterminate,
    #[error("UsageRecordTooLarge")]
    RecordTooLarge,
    #[error("UsageLockAbandoned")]
    LockAbandoned,
    #[error("DurableLayoutFailed")]
    LayoutFailed,
    #[error("InvalidGenerationFact")]
    InvalidFact,
    #[error("InvalidPendingMarker")]
    InvalidPending,
    #[error("InvalidUsageIncident")]
    InvalidIncident,
    #[error("AccessDenied")]
    AccessDenied,
    #[error("PermissionDenied")]
    PermissionDenied,
    #[error("Unexpected")]
    Unexpected,
}

impl From<Errno> for UsageStoreError {
    fn from(errno: Errno) -> Self {
        match errno {
            Errno::ACCESS => Self::AccessDenied,
            Errno::PERM => Self::PermissionDenied,
            _ => Self::Unexpected,
        }
    }
}

impl From<DurableError> for UsageStoreError {
    fn from(error: DurableError) -> Self {
        match error {
            DurableError::PathUnsafe => Self::PathUnsafe,
            DurableError::PermissionsUnsupported => Self::PermissionsUnsupported,
            DurableError::AccessDenied => Self::AccessDenied,
            _ => Self::Unexpected,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct LoadedUsage {
    pub(crate) coverage_started_at_ms: Option<i64>,
    pub(crate) facts: Vec<GenerationFact>,
    pub(crate) pending: Vec<PendingMarker>,
    pub(crate) incidents: Vec<UsageIncident>,
}

pub(crate) struct ProfileUsageStore {
    data_dir: PathBuf,
    home: Option<PrivateDir>,
    lock_deadline: Duration,
    index: Option<RecordIndex>,
    abandoned: Arc<AtomicBool>,
}

impl ProfileUsageStore {
    pub(crate) fn open(data_dir: &Path) -> Result<Self, UsageStoreError> {
        Ok(Self {
            data_dir: data_dir.to_owned(),
            home: PrivateDir::open_existing(data_dir)?,
            lock_deadline: LOCK_DEADLINE,
            index: None,
            abandoned: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn abandon_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.abandoned)
    }

    pub(crate) fn load(&mut self) -> Result<LoadedUsage, UsageStoreError> {
        if self.home.is_none() {
            self.home = PrivateDir::open_existing(&self.data_dir)?;
        }
        let Some(home) = &self.home else {
            return Ok(LoadedUsage::default());
        };
        validate_readable(home)?;
        loop {
            if let Some(_lock) = lock_existing(home, self.lock_deadline)? {
                return load_unlocked(home);
            }
            let loaded = load_unlocked(home)?;
            if !lock_file_exists(home)? {
                return Ok(loaded);
            }
        }
    }
}

fn validate_readable(home: &PrivateDir) -> Result<(), UsageStoreError> {
    let stat = fs::fstat(home.as_fd())?;
    if file_type(&stat) != FileType::Directory {
        return Err(UsageStoreError::PathUnsafe);
    }
    if permissions(&stat) != Mode::RWXU {
        return Err(UsageStoreError::PermissionsUnsupported);
    }
    Ok(())
}

fn lock_existing(
    home: &PrivateDir,
    deadline: Duration,
) -> Result<Option<OwnedFd>, UsageStoreError> {
    let flags =
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let lock = match fs::openat(home.as_fd(), USAGE_LOCK_FILE, flags, Mode::empty()) {
        Ok(lock) => lock,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::ISDIR | Errno::NOTDIR) => return Err(UsageStoreError::PathUnsafe),
        Err(errno) => return Err(errno.into()),
    };
    verify_private_file(&fs::fstat(&lock)?)?;
    let started = Instant::now();
    loop {
        match fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Some(lock)),
            Err(Errno::WOULDBLOCK | Errno::INTR) => {}
            Err(Errno::NOLCK | Errno::OPNOTSUPP) => return Err(UsageStoreError::LockUnsupported),
            Err(errno) => return Err(errno.into()),
        }
        if started.elapsed() >= deadline {
            return Err(UsageStoreError::LockBusy);
        }
        thread::sleep(LOCK_RETRY.min(deadline));
    }
}

fn lock_file_exists(home: &PrivateDir) -> Result<bool, UsageStoreError> {
    match fs::statat(home.as_fd(), USAGE_LOCK_FILE, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => verify_private_file(&stat).map(|()| true),
        Err(Errno::NOENT) => Ok(false),
        Err(Errno::LOOP | Errno::NOTDIR) => Err(UsageStoreError::PathUnsafe),
        Err(errno) => Err(errno.into()),
    }
}

fn verify_private_file(stat: &Stat) -> Result<(), UsageStoreError> {
    if file_type(stat) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(UsageStoreError::PathUnsafe);
    }
    if permissions(stat) != private_file_mode() {
        return Err(UsageStoreError::PermissionsUnsupported);
    }
    Ok(())
}

fn load_unlocked(home: &PrivateDir) -> Result<LoadedUsage, UsageStoreError> {
    let Some(file) = open_usage_read_only(home)? else {
        return Ok(LoadedUsage::default());
    };
    let boundary = fs::fstat(&file)?.st_size;
    let boundary = u64::try_from(boundary).map_err(|_| UsageStoreError::ReadFailed)?;
    if boundary > MAX_FILE_BYTES {
        return Err(UsageStoreError::CapacityExceeded);
    }
    if boundary == 0 {
        return Ok(LoadedUsage::default());
    }
    let length = usize::try_from(boundary).map_err(|_| UsageStoreError::CapacityExceeded)?;
    let mut bytes = vec![0; length];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|_| UsageStoreError::ReadFailed)?;
    if bytes.last() != Some(&b'\n') {
        return Err(UsageStoreError::Incomplete);
    }
    let mut index = RecordIndex::default();
    index.absorb_bytes(&bytes, None)?;
    Ok(index.loaded)
}

fn open_usage_read_only(home: &PrivateDir) -> Result<Option<File>, UsageStoreError> {
    match fs::statat(home.as_fd(), USAGE_FILE, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => verify_read_only_regular(&stat)?,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => return Err(UsageStoreError::PathUnsafe),
        Err(errno) => return Err(errno.into()),
    }
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let file = match fs::openat(home.as_fd(), USAGE_FILE, flags, Mode::empty()) {
        Ok(file) => file,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::ISDIR | Errno::NOTDIR | Errno::NXIO) => {
            return Err(UsageStoreError::PathUnsafe);
        }
        Err(errno) => return Err(errno.into()),
    };
    let stat = fs::fstat(&file)?;
    verify_read_only_regular(&stat)?;
    if permissions(&stat) != private_file_mode() {
        return Err(UsageStoreError::PermissionsUnsupported);
    }
    Ok(Some(File::from(file)))
}

fn verify_read_only_regular(stat: &Stat) -> Result<(), UsageStoreError> {
    if file_type(stat) != FileType::RegularFile || stat.st_nlink > 1 {
        return Err(UsageStoreError::PathUnsafe);
    }
    Ok(())
}

#[derive(Default)]
struct RecordIndex {
    loaded: LoadedUsage,
    fact_variants: HashMap<String, Variants>,
    pending_variants: HashMap<String, Variants>,
    record_count: usize,
    boundary: u64,
    tail_sample: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Variants {
    first: usize,
    second: Option<usize>,
}

enum ParsedRecord {
    Coverage(i64),
    Generation(GenerationFact),
    Pending(PendingMarker),
    Incident(UsageIncident),
}

impl RecordIndex {
    fn absorb_bytes(
        &mut self,
        bytes: &[u8],
        abandoned: Option<&AtomicBool>,
    ) -> Result<(), UsageStoreError> {
        let lines = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty());
        for (parsed, line) in lines.enumerate() {
            if parsed % ABANDON_CHECK_LINES == 0
                && abandoned.is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                return Err(UsageStoreError::LockAbandoned);
            }
            self.record_count += 1;
            if self.record_count > MAX_RECORDS || line.len() > MAX_RECORD_BYTES {
                return Err(UsageStoreError::CapacityExceeded);
            }
            let record = parse_record(line).ok_or(UsageStoreError::Invalid)?;
            self.absorb(record)?;
        }
        Ok(())
    }

    fn absorb(&mut self, record: ParsedRecord) -> Result<(), UsageStoreError> {
        let loaded = &mut self.loaded;
        match record {
            ParsedRecord::Coverage(started_at_ms) => match loaded.coverage_started_at_ms {
                Some(existing) if existing != started_at_ms => {
                    return Err(UsageStoreError::Invalid);
                }
                Some(_) => {}
                None => loaded.coverage_started_at_ms = Some(started_at_ms),
            },
            ParsedRecord::Generation(fact) => {
                if loaded.coverage_started_at_ms.is_none() {
                    return Err(UsageStoreError::Invalid);
                }
                let id = fact.id.clone();
                absorb_variant(&mut loaded.facts, &mut self.fact_variants, id, fact);
            }
            ParsedRecord::Pending(marker) => {
                if loaded.coverage_started_at_ms.is_none() {
                    return Err(UsageStoreError::Invalid);
                }
                let id = marker.id.clone();
                let observed_at_ms = marker.observed_at_ms;
                if absorb_variant(&mut loaded.pending, &mut self.pending_variants, id, marker) {
                    loaded.incidents.push(UsageIncident {
                        occurred_at_ms: observed_at_ms,
                        completeness: UsageCompleteness::Incomplete,
                    });
                }
            }
            ParsedRecord::Incident(incident) => loaded.incidents.push(incident),
        }
        Ok(())
    }
}

fn absorb_variant<T: PartialEq>(
    records: &mut Vec<T>,
    variants: &mut HashMap<String, Variants>,
    id: String,
    record: T,
) -> bool {
    let Some(known) = variants.get_mut(&id) else {
        records.push(record);
        variants.insert(
            id,
            Variants {
                first: records.len() - 1,
                second: None,
            },
        );
        return false;
    };
    if known.second.is_some() || records[known.first] == record {
        return false;
    }
    records.push(record);
    known.second = Some(records.len() - 1);
    true
}

fn parse_record(line: &[u8]) -> Option<ParsedRecord> {
    let value = parse_json(line).ok()?;
    let object = value.as_object()?;
    let schema_version = object.get("schema_version")?.as_u64()?;
    let kind = object.get("kind")?.as_str()?;
    if schema_version != SCHEMA_VERSION {
        return None;
    }
    let field = |name: &str| object.get(name);
    match (kind, object.len()) {
        ("coverage", COVERAGE_FIELDS) => Some(ParsedRecord::Coverage(non_negative(field(
            "started_at_ms",
        )?)?)),
        ("pending", PENDING_FIELDS) => {
            let marker = PendingMarker {
                id: field("id")?.as_str()?.to_owned(),
                observed_at_ms: non_negative(field("observed_at_ms")?)?,
            };
            marker.is_valid().then_some(ParsedRecord::Pending(marker))
        }
        ("incident", INCIDENT_FIELDS) => {
            let completeness = UsageCompleteness::parse(field("completeness")?.as_str()?)?;
            matches!(
                completeness,
                UsageCompleteness::Pending | UsageCompleteness::Incomplete
            )
            .then_some(())?;
            Some(ParsedRecord::Incident(UsageIncident {
                occurred_at_ms: non_negative(field("occurred_at_ms")?)?,
                completeness,
            }))
        }
        ("generation", GENERATION_FIELDS) => {
            generation_fact_codec::parse(field("fact")?).map(ParsedRecord::Generation)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
