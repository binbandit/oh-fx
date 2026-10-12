use std::fmt;
use std::os::fd::OwnedFd;

use super::blobs::{BlobError, read_blob};
use super::fold::State;
use super::log::{ForwardReader, ReadError, last_line_end};
use super::schema::{Body, parse_line_body, valid_id};
use super::storage::{self, IoFault, StorageError};
use ofx_config::PrivateDir;

const LOG_NAME: &str = "log.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Verified {
    pub(crate) damaged_at: Option<u64>,
    pub(crate) bad_snapshots: u64,
    pub(crate) bad_blobs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerifyError {
    NotFound,
    InvalidArgument,
    Fault(IoFault),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("NotFound"),
            Self::InvalidArgument => formatter.write_str("InvalidArgument"),
            Self::Fault(fault) => fault.fmt(formatter),
        }
    }
}

impl From<StorageError> for VerifyError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound => Self::NotFound,
            StorageError::Fault(fault) => Self::Fault(fault),
        }
    }
}

pub(crate) fn verify_session(root: &PrivateDir, id: &str) -> Result<Verified, VerifyError> {
    if !valid_id(id) {
        return Err(VerifyError::NotFound);
    }
    let session = storage::open_dir(root, id)?;
    let log = storage::open_file(&session, LOG_NAME)?;
    let len = log.metadata().map_err(StorageError::from)?.len();
    let end = last_line_end(&log, len).map_err(|error| match error {
        ReadError::Corrupt(_) => VerifyError::Fault(IoFault::Io),
        ReadError::Fault(fault) => VerifyError::Fault(fault),
    })?;
    let mut reader = ForwardReader::new(&log, 0, end, Some(1));
    let mut state = State::default();
    let mut verified = Verified {
        damaged_at: None,
        bad_snapshots: 0,
        bad_blobs: 0,
    };
    let mut fork_seq = 0;
    loop {
        let line = match reader.next() {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(ReadError::Corrupt(offset)) => {
                verified.damaged_at = Some(offset);
                break;
            }
            Err(ReadError::Fault(fault)) => return Err(VerifyError::Fault(fault)),
        };
        let Some(kind) = line.header.kind else {
            continue;
        };
        let Ok(body) = parse_line_body(kind, line.bytes) else {
            verified.damaged_at = Some(line.offset);
            break;
        };
        for hash in body.blob_refs() {
            if !blob_is_whole(&session, hash)? {
                verified.bad_blobs += 1;
            }
        }
        match &body {
            Body::SessionCreated {
                fork_seq: origin, ..
            } => fork_seq = *origin,
            Body::Snapshot { state: raw } => {
                let Ok(decoded) = State::decode(raw) else {
                    verified.bad_snapshots += 1;
                    continue;
                };
                let mut decoded = decoded.with_position_of(&state);
                if line.header.seq <= fork_seq {
                    decoded.clear_children();
                }
                if decoded != state {
                    verified.bad_snapshots += 1;
                }
            }
            _ => {}
        }
        state.apply(fork_seq, line.header.seq, line.offset, &body);
    }
    Ok(verified)
}

fn blob_is_whole(session: &OwnedFd, hash: &str) -> Result<bool, VerifyError> {
    match read_blob(session, hash) {
        Ok(_) => Ok(true),
        Err(BlobError::NotFound | BlobError::Corrupt) => Ok(false),
        Err(BlobError::Fault(fault)) => Err(VerifyError::Fault(fault)),
    }
}

#[cfg(test)]
mod tests;
