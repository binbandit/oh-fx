use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_text::lowercase_hex;

pub const TAPE_MAGIC: &[u8] = b"FXTP\x01";
const HEADER_LEN: usize = TAPE_MAGIC.len() + 2 + 2 + 8 + 1;
const FRAME_HEADER_LEN: usize = 9;
const MAX_VERSION_LEN: usize = 255;
const RECORDINGS_DIR: &str = "recordings";
const TEMPORARY_RECORDINGS_DIR: &str = "oh-fx-recordings";
const DEFAULT_TEMPORARY_DIR: &str = "/tmp";
const TAPE_PREFIX: &str = "oh-fx-record-";
const TAPE_EXTENSION: &str = ".fxtape";
const RANDOM_BYTES: usize = 6;
const AUTOMATIC_ATTEMPTS: usize = 8;
const PRIVATE_TAPE_MODE: u32 = 0o600;
const DEBUG_RECORD_VARIABLE: &str = "OH_FX_DEBUG_RECORD";
const RECORD_VARIABLE: &str = "OH_FX_RECORD";
const RECORD_INPUT_VARIABLE: &str = "OH_FX_RECORD_INPUT";
const SILENT_BANNER_VARIABLE: &str = "OH_FX_DEBUG_RECORD_SILENT_BANNER";
const TEMPORARY_DIR_VARIABLE: &str = "TMPDIR";
const ENVIRONMENT_TRIM: &[char] = &[' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapeKind {
    Stdout,
    Stdin,
    Resize,
    Sigint,
    Marker,
    Unknown(u8),
}

impl TapeKind {
    pub const fn from_byte(byte: u8) -> Self {
        match byte {
            1 => Self::Stdout,
            2 => Self::Stdin,
            3 => Self::Resize,
            4 => Self::Sigint,
            5 => Self::Marker,
            other => Self::Unknown(other),
        }
    }

    const fn byte(self) -> u8 {
        match self {
            Self::Stdout => 1,
            Self::Stdin => 2,
            Self::Resize => 3,
            Self::Sigint => 4,
            Self::Marker => 5,
            Self::Unknown(byte) => byte,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stdin => "stdin",
            Self::Resize => "resize",
            Self::Sigint => "sigint",
            Self::Marker => "marker",
            Self::Unknown(_) => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TapeError {
    #[error("TapeTooShort")]
    TapeTooShort,
    #[error("BadTapeMagic")]
    BadTapeMagic,
    #[error("TruncatedVersion")]
    TruncatedVersion,
    #[error("TruncatedFrameHeader")]
    TruncatedFrameHeader,
    #[error("TruncatedFramePayload")]
    TruncatedFramePayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeHeader<'a> {
    pub cols: u16,
    pub rows: u16,
    pub epoch_ms: i64,
    pub version: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeFrame<'a> {
    pub delta_ms: i32,
    pub kind: TapeKind,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct TapeParser<'a> {
    bytes: &'a [u8],
    position: usize,
    pub header: TapeHeader<'a>,
}

impl<'a> TapeParser<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, TapeError> {
        if bytes.len() < HEADER_LEN {
            return Err(TapeError::TapeTooShort);
        }
        if !bytes.starts_with(TAPE_MAGIC) {
            return Err(TapeError::BadTapeMagic);
        }
        let fixed = &bytes[TAPE_MAGIC.len()..HEADER_LEN];
        let version_len = usize::from(fixed[12]);
        let version = bytes
            .get(HEADER_LEN..HEADER_LEN + version_len)
            .ok_or(TapeError::TruncatedVersion)?;
        Ok(Self {
            bytes,
            position: HEADER_LEN + version_len,
            header: TapeHeader {
                cols: u16::from_le_bytes([fixed[0], fixed[1]]),
                rows: u16::from_le_bytes([fixed[2], fixed[3]]),
                epoch_ms: i64::from_le_bytes(fixed[4..12].try_into().unwrap_or_default()),
                version,
            },
        })
    }

    pub fn next_frame(&mut self) -> Result<Option<TapeFrame<'a>>, TapeError> {
        let rest = &self.bytes[self.position..];
        if rest.is_empty() {
            return Ok(None);
        }
        let fixed = rest
            .get(..FRAME_HEADER_LEN)
            .ok_or(TapeError::TruncatedFrameHeader)?;
        let payload_len = u32::from_le_bytes([fixed[5], fixed[6], fixed[7], fixed[8]]);
        let payload = usize::try_from(payload_len)
            .ok()
            .and_then(|len| rest.get(FRAME_HEADER_LEN..FRAME_HEADER_LEN.checked_add(len)?))
            .ok_or(TapeError::TruncatedFramePayload)?;
        self.position += FRAME_HEADER_LEN + payload.len();
        Ok(Some(TapeFrame {
            delta_ms: i32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]),
            kind: TapeKind::from_byte(fixed[4]),
            payload,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingDestination {
    Inactive,
    Automatic,
    Explicit(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingPolicy {
    pub destination: RecordingDestination,
    pub record_stdin: bool,
    pub strict_start: bool,
    pub show_inline_notice: bool,
}

pub fn recording_policy(lookup: &dyn Fn(&str) -> Option<String>) -> RecordingPolicy {
    let debug_record = debug_truthy(lookup(DEBUG_RECORD_VARIABLE).as_deref());
    let configured = lookup(RECORD_VARIABLE)
        .map(|path| path.trim_matches(ENVIRONMENT_TRIM).to_owned())
        .filter(|path| !path.is_empty());
    let destination = match configured {
        Some(path) => RecordingDestination::Explicit(path),
        None if debug_record => RecordingDestination::Automatic,
        None => RecordingDestination::Inactive,
    };
    let active = destination != RecordingDestination::Inactive;
    RecordingPolicy {
        destination,
        record_stdin: active && input_truthy(lookup(RECORD_INPUT_VARIABLE).as_deref()),
        strict_start: debug_record,
        show_inline_notice: active && !debug_truthy(lookup(SILENT_BANNER_VARIABLE).as_deref()),
    }
}

fn debug_truthy(value: Option<&str>) -> bool {
    matches_any(value, &["1", "true", "yes", "on"])
}

fn input_truthy(value: Option<&str>) -> bool {
    matches_any(value, &["1", "true", "on"])
}

fn matches_any(value: Option<&str>, accepted: &[&str]) -> bool {
    value.is_some_and(|value| {
        let trimmed = value.trim_matches(ENVIRONMENT_TRIM);
        accepted
            .iter()
            .any(|accepted| trimmed.eq_ignore_ascii_case(accepted))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureStatus {
    Inactive,
    Active {
        path: PathBuf,
        show_inline_notice: bool,
    },
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Replace,
    Private,
}

struct ActiveTape {
    file: File,
    path: PathBuf,
    last_ms: i64,
    record_stdin: bool,
    show_inline_notice: bool,
}

enum Capture {
    Active(ActiveTape),
    Failed,
    Closed,
}

pub struct TapeRecorder {
    capture: Mutex<Capture>,
}

impl TapeRecorder {
    pub fn start(
        lookup: &dyn Fn(&str) -> Option<String>,
        state_dir: Option<&Path>,
        cols: u16,
        rows: u16,
        version: &str,
    ) -> io::Result<Option<Self>> {
        let policy = recording_policy(lookup);
        let recorder = match &policy.destination {
            RecordingDestination::Inactive => return Ok(None),
            RecordingDestination::Automatic => {
                let root = match state_dir {
                    Some(state) => state.join(RECORDINGS_DIR),
                    None => PathBuf::from(
                        lookup(TEMPORARY_DIR_VARIABLE)
                            .unwrap_or_else(|| DEFAULT_TEMPORARY_DIR.to_owned()),
                    )
                    .join(TEMPORARY_RECORDINGS_DIR),
                };
                Self::open_automatic(&root, cols, rows, version, policy.show_inline_notice)?
            }
            RecordingDestination::Explicit(path) => match Self::open(
                Path::new(path),
                cols,
                rows,
                version,
                Placement::Replace,
                policy.show_inline_notice,
            ) {
                Ok(recorder) => recorder,
                Err(error) if policy.strict_start => return Err(error),
                Err(_) => return Ok(None),
            },
        };
        if policy.record_stdin
            && let Capture::Active(tape) = &mut *recorder.lock()
        {
            tape.record_stdin = true;
        }
        Ok(Some(recorder))
    }

    fn open_automatic(
        root: &Path,
        cols: u16,
        rows: u16,
        version: &str,
        show_inline_notice: bool,
    ) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        for _ in 0..AUTOMATIC_ATTEMPTS {
            let mut random = [0_u8; RANDOM_BYTES];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let name = format!(
                "{TAPE_PREFIX}{}-{}{TAPE_EXTENSION}",
                now_ms(),
                lowercase_hex(&random)
            );
            match Self::open(
                &root.join(name),
                cols,
                rows,
                version,
                Placement::Private,
                show_inline_notice,
            ) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                opened => return opened,
            }
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }

    fn open(
        path: &Path,
        cols: u16,
        rows: u16,
        version: &str,
        placement: Placement,
        show_inline_notice: bool,
    ) -> io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.write(true);
        match placement {
            Placement::Replace => options.create(true).truncate(true),
            Placement::Private => options.create_new(true).mode(PRIVATE_TAPE_MODE),
        };
        let mut file = options.open(path)?;
        let now = now_ms();
        file.write_all(&header(cols, rows, now, version))?;
        Ok(Self {
            capture: Mutex::new(Capture::Active(ActiveTape {
                file,
                path: path.to_owned(),
                last_ms: now,
                record_stdin: false,
                show_inline_notice,
            })),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Capture> {
        self.capture.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn record_stdout(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.write_frame(TapeKind::Stdout, bytes, false);
        }
    }

    pub fn record_stdin(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.write_frame(TapeKind::Stdin, bytes, true);
        }
    }

    pub fn record_resize(&self, cols: u16, rows: u16) {
        let mut payload = [0_u8; 4];
        payload[..2].copy_from_slice(&cols.to_le_bytes());
        payload[2..].copy_from_slice(&rows.to_le_bytes());
        self.write_frame(TapeKind::Resize, &payload, false);
    }

    fn write_frame(&self, kind: TapeKind, payload: &[u8], input: bool) {
        let mut capture = self.lock();
        let Capture::Active(tape) = &mut *capture else {
            return;
        };
        if input && !tape.record_stdin {
            return;
        }
        let now = now_ms();
        let delta = i32::try_from(now.saturating_sub(tape.last_ms).max(0)).unwrap_or(i32::MAX);
        tape.last_ms = now;
        let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        let payload = &payload[..usize::try_from(len).unwrap_or(payload.len())];
        let mut fixed = [0_u8; FRAME_HEADER_LEN];
        fixed[..4].copy_from_slice(&delta.to_le_bytes());
        fixed[4] = kind.byte();
        fixed[5..].copy_from_slice(&len.to_le_bytes());
        let written = tape
            .file
            .write_all(&fixed)
            .and_then(|()| tape.file.write_all(payload));
        if written.is_err() {
            *capture = Capture::Failed;
        }
    }

    pub fn shutdown(&self) {
        let mut capture = self.lock();
        if matches!(*capture, Capture::Active(_)) {
            *capture = Capture::Closed;
        }
    }

    pub fn capture_status(&self) -> CaptureStatus {
        match &*self.lock() {
            Capture::Active(tape) => CaptureStatus::Active {
                path: tape.path.clone(),
                show_inline_notice: tape.show_inline_notice,
            },
            Capture::Failed => CaptureStatus::Failed,
            Capture::Closed => CaptureStatus::Inactive,
        }
    }
}

fn header(cols: u16, rows: u16, epoch_ms: i64, version: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_LEN + version.len());
    bytes.extend_from_slice(TAPE_MAGIC);
    bytes.extend_from_slice(&cols.to_le_bytes());
    bytes.extend_from_slice(&rows.to_le_bytes());
    bytes.extend_from_slice(&epoch_ms.to_le_bytes());
    bytes.push(u8::try_from(version.len().min(MAX_VERSION_LEN)).unwrap_or(u8::MAX));
    bytes.extend_from_slice(version.as_bytes());
    bytes
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
