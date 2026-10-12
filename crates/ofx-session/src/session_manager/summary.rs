use std::fs::File;
use std::os::fd::AsFd;

use super::fold::State;
use super::log::{BackwardReader, ForwardReader, ReadError, last_line_end};
use super::schema::{Body, Kind, Role, parse_line_body, valid_id};
use super::storage::{self, IoFault, StorageError};

const LOG_NAME: &str = "log.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenError {
    NotFound,
    Corrupt,
    Fault(IoFault),
}

impl From<StorageError> for OpenError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound => Self::NotFound,
            StorageError::Fault(fault) => Self::Fault(fault),
        }
    }
}

impl From<ReadError> for OpenError {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::Corrupt(_) => Self::Corrupt,
            ReadError::Fault(fault) => Self::Fault(fault),
        }
    }
}

pub(crate) struct Peeked {
    pub(crate) role: Role,
    pub(crate) state: State,
    pub(crate) created_ms: u64,
    pub(crate) updated_ms: u64,
}

pub(crate) fn open_log(root: impl AsFd, id: &str) -> Result<(File, u64), OpenError> {
    if !valid_id(id) {
        return Err(OpenError::NotFound);
    }
    let session = storage::open_dir(root, id)?;
    let log = storage::open_file(&session, LOG_NAME)?;
    let len = log.metadata().map_err(StorageError::from)?.len();
    let end = last_line_end(&log, len).map_err(|_| OpenError::Corrupt)?;
    Ok((log, end))
}

pub(crate) fn read_summary(root: impl AsFd, id: &str) -> Result<Peeked, OpenError> {
    let (log, end) = open_log(root, id)?;
    let mut first = ForwardReader::new(&log, 0, end, Some(1));
    let line1 = first.next()?.ok_or(OpenError::Corrupt)?;
    if line1.header.kind != Some(Kind::SessionCreated) {
        return Err(OpenError::Corrupt);
    }
    let Ok(Body::SessionCreated { role, fork_seq }) =
        parse_line_body(Kind::SessionCreated, line1.bytes)
    else {
        return Err(OpenError::Corrupt);
    };
    let line1_end = line1.offset + line1.bytes.len() as u64;
    let created_ms = line1.header.ts_ms;
    let mut state = State::default().at_seq(1);
    let mut from = line1_end;
    let mut expected = 2;
    let mut updated_ms = created_ms;
    let mut back = BackwardReader::new(&log, end);
    while let Some(line) = back.next()? {
        if line.offset < line1_end {
            break;
        }
        if line.header.kind != Some(Kind::Snapshot) {
            continue;
        }
        let Ok(Body::Snapshot { state: raw }) = parse_line_body(Kind::Snapshot, line.bytes) else {
            return Err(OpenError::Corrupt);
        };
        state = State::decode(raw)
            .map_err(|_| OpenError::Corrupt)?
            .at_seq(line.header.seq);
        if line.header.seq <= fork_seq {
            state.clear_children();
        }
        from = line.offset + line.bytes.len() as u64;
        expected = line.header.seq + 1;
        updated_ms = line.header.ts_ms;
        break;
    }
    let mut tail = ForwardReader::new(&log, from, end, Some(expected));
    while let Some(line) = tail.next()? {
        updated_ms = line.header.ts_ms;
        let Some(kind) = line.header.kind else {
            state.skip_unknown(line.header.seq);
            continue;
        };
        let body = parse_line_body(kind, line.bytes).map_err(|_| OpenError::Corrupt)?;
        state.apply(fork_seq, line.header.seq, line.offset, &body);
    }
    Ok(Peeked {
        role,
        state,
        created_ms,
        updated_ms: updated_ms.max(created_ms),
    })
}

pub(crate) fn read_forward<E>(
    root: impl AsFd,
    id: &str,
    mut visit: impl FnMut(u64, Option<Body<'_>>) -> Result<(), E>,
) -> Result<Result<bool, E>, OpenError> {
    let (log, end) = open_log(root, id)?;
    let mut reader = ForwardReader::new(&log, 0, end, Some(1));
    loop {
        let line = match reader.next() {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(Ok(false)),
            Err(ReadError::Corrupt(_)) => return Ok(Ok(true)),
            Err(ReadError::Fault(fault)) => return Err(OpenError::Fault(fault)),
        };
        let body = line
            .header
            .kind
            .and_then(|kind| parse_line_body(kind, line.bytes).ok());
        if let Err(error) = visit(line.header.seq, body) {
            return Ok(Err(error));
        }
    }
}

#[cfg(test)]
mod tests;
