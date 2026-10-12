use std::fmt;
use std::fs::File;
use std::mem;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_text::lowercase_hex;
use ofx_trace::trace_log;
use rustix::fs::Mode;
use sha2::{Digest, Sha256};

use super::catalog::{self, Summary};
use super::encode::{Identity, Line, append_created, append_line};
use super::fields::{Tag, full_string};
use super::fold::{Event, State};
use super::json::value_end;
use super::schema::{Host, Reason, Role, SetKey, valid_blob_hash};
use super::storage::{self, IoFault, io_fault};
use crate::session_layout::generate_session_id;
use crate::session_trace::SESSION;

const MAX_TEXT_BYTES: usize = 4096;
const MAX_VALUE_BYTES: usize = 4_128_768;
const MAX_LANGUAGE_BYTES: usize = 24;
const MAX_BLOB_BYTES: usize = 512 << 20;
const MAX_ITEM_TYPE_BYTES: usize = 32;
const TMP_DIR: &str = ".tmp";
const BLOBS_DIR: &str = "blobs";
const LOG_NAME: &str = "log.jsonl";
const LOCK_NAME: &str = "lock";
const PENDING_SUFFIX_BYTES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppendError {
    InvalidArgument,
    InvalidTransition,
    SessionClosed,
    TooLarge,
    Fault(IoFault),
}

impl fmt::Display for AppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument => formatter.write_str("InvalidArgument"),
            Self::InvalidTransition => formatter.write_str("InvalidTransition"),
            Self::SessionClosed => formatter.write_str("SessionClosed"),
            Self::TooLarge => formatter.write_str("TooLarge"),
            Self::Fault(fault) => fault.fmt(formatter),
        }
    }
}

impl From<IoFault> for AppendError {
    fn from(fault: IoFault) -> Self {
        Self::Fault(fault)
    }
}

struct Live {
    root: OwnedFd,
    blobs: OwnedFd,
    log: File,
    lock: File,
    end: u64,
    next_seq: u64,
}

enum Phase {
    Held,
    Live(Live),
    Failed(Option<Live>),
    Closed,
}

pub(crate) struct Session {
    root_path: PathBuf,
    identity: Identity,
    state: State,
    phase: Phase,
    fault: Option<IoFault>,
    created_ms: Option<u64>,
    updated_ms: u64,
    held: Vec<u8>,
    held_lines: u64,
    #[cfg(test)]
    fail_next_write: Option<IoFault>,
}

enum Listing {
    Published,
    Changed,
}

impl Session {
    pub(crate) fn open_new(
        root_path: &Path,
        workspace: &str,
        host: Host,
    ) -> Result<Self, AppendError> {
        if workspace.is_empty() || workspace.len() > MAX_TEXT_BYTES {
            return Err(AppendError::InvalidArgument);
        }
        Ok(Self {
            root_path: root_path.to_path_buf(),
            identity: Identity {
                id: generate_session_id().ok_or(AppendError::Fault(IoFault::Io))?,
                workspace: workspace.to_owned(),
                role: Role::Root,
                host,
            },
            state: State::default(),
            phase: Phase::Held,
            fault: None,
            created_ms: None,
            updated_ms: 0,
            held: Vec::new(),
            held_lines: 0,
            #[cfg(test)]
            fail_next_write: None,
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.identity.id
    }

    pub(crate) fn failure(&self) -> Option<AppendError> {
        match self.phase {
            Phase::Held | Phase::Live(_) => None,
            Phase::Failed(_) => Some(self.fault.unwrap_or(IoFault::Io).into()),
            Phase::Closed => Some(AppendError::SessionClosed),
        }
    }

    pub(crate) fn append(&mut self, events: &[Event<'_>]) -> Result<u64, AppendError> {
        check_events(events)?;
        let was_held = matches!(self.phase, Phase::Held);
        let last_seq = self.append_lines(events)?;
        if was_held && matches!(self.phase, Phase::Live(_)) {
            self.update_index(&Listing::Published);
        } else if matches!(self.phase, Phase::Live(_)) && changes_listing(events) {
            self.update_index(&Listing::Changed);
        }
        Ok(last_seq)
    }

    fn append_lines(&mut self, events: &[Event<'_>]) -> Result<u64, AppendError> {
        match &self.phase {
            Phase::Closed => return Err(AppendError::SessionClosed),
            Phase::Failed(_) => return Err(self.fault.unwrap_or(IoFault::Io).into()),
            Phase::Held | Phase::Live(_) => {}
        }
        let lines = self
            .state
            .plan(events)
            .map_err(|_| AppendError::InvalidTransition)?;
        let ts_ms = now_ms();
        if matches!(self.phase, Phase::Held) {
            if lines.iter().any(|line| !line.names_blobs().is_empty()) {
                return Err(AppendError::InvalidTransition);
            }
            if lines
                .iter()
                .any(|line| matches!(line, Line::TurnStarted(_)))
            {
                self.publish(&lines, ts_ms)?;
            } else {
                self.hold(&lines, ts_ms)?;
            }
            return Ok(self.state.last_seq());
        }
        self.check_blob_refs(&lines)?;
        self.write_lines(&lines, ts_ms)?;
        if needs_sync(&lines) {
            self.sync()?;
        }
        Ok(self.state.last_seq())
    }

    fn hold(&mut self, lines: &[Line<'_>], ts_ms: u64) -> Result<(), AppendError> {
        if !lines.iter().all(|line| matches!(line, Line::Set(..))) {
            return Err(AppendError::InvalidTransition);
        }
        let start = self.held.len();
        for (seq, line) in (2 + self.held_lines..).zip(lines) {
            if append_line(&mut self.held, seq, ts_ms, line).is_err() {
                self.held.truncate(start);
                return Err(AppendError::TooLarge);
            }
        }
        for (seq, line) in (2 + self.held_lines..).zip(lines) {
            self.state.apply(0, seq, 0, &line.body());
        }
        self.held_lines += lines.len() as u64;
        Ok(())
    }

    fn publish(&mut self, lines: &[Line<'_>], ts_ms: u64) -> Result<(), AppendError> {
        let root = storage::open_root(&self.root_path)?;
        let created_ms = self.created_ms.unwrap_or(ts_ms);
        let first_seq = 2 + self.held_lines;
        let mut starts = Vec::with_capacity(lines.len());
        let staged = self
            .frame_publish(created_ms, lines, ts_ms, &mut starts)
            .and_then(|batch| Ok(self.stage(root, &batch, first_seq + lines.len() as u64)?));
        let live = match staged {
            Ok(live) => live,
            Err(error) => {
                self.phase = Phase::Failed(None);
                if let AppendError::Fault(fault) = error {
                    self.fault.get_or_insert(fault);
                }
                return Err(error);
            }
        };
        self.created_ms = Some(created_ms);
        for ((seq, line), offset) in (first_seq..).zip(lines).zip(starts) {
            self.state.apply(0, seq, offset, &line.body());
        }
        self.phase = Phase::Live(live);
        self.wrote(ts_ms);
        self.held = Vec::new();
        self.held_lines = 0;
        Ok(())
    }

    fn frame_publish(
        &self,
        created_ms: u64,
        lines: &[Line<'_>],
        ts_ms: u64,
        starts: &mut Vec<u64>,
    ) -> Result<Vec<u8>, AppendError> {
        let mut batch = Vec::new();
        append_created(&mut batch, created_ms, &self.identity)
            .map_err(|_| AppendError::TooLarge)?;
        batch.extend_from_slice(&self.held);
        for (seq, line) in (2 + self.held_lines..).zip(lines) {
            starts.push(batch.len() as u64);
            append_line(&mut batch, seq, ts_ms, line).map_err(|_| AppendError::TooLarge)?;
        }
        Ok(batch)
    }

    fn stage(&self, root: OwnedFd, batch: &[u8], next_seq: u64) -> Result<Live, IoFault> {
        let id = self.identity.id.as_str();
        let tmp = storage::ensure_dir(&root, TMP_DIR)?;
        storage::make_dir(&tmp, id)?;
        let dir = storage::ensure_dir(&tmp, id)?;
        let blobs = storage::ensure_dir(&dir, BLOBS_DIR)?;
        let log = storage::create_file(&dir, LOG_NAME, Mode::RUSR | Mode::WUSR)?;
        log.write_all_at(batch, 0).map_err(io_fault)?;
        log.sync_all().map_err(io_fault)?;
        let lock = storage::create_file(&dir, LOCK_NAME, Mode::RUSR | Mode::WUSR)?;
        if !storage::try_lock(&lock)? {
            return Err(IoFault::Io);
        }
        storage::sync_dir(&dir)?;
        storage::rename_into(&tmp, id, &root)?;
        storage::sync_dir(&root)?;
        Ok(Live {
            root,
            blobs,
            log,
            lock,
            end: batch.len() as u64,
            next_seq,
        })
    }

    fn check_blob_refs(&self, lines: &[Line<'_>]) -> Result<(), AppendError> {
        let Phase::Live(live) = &self.phase else {
            return Err(AppendError::SessionClosed);
        };
        let named = lines.iter().flat_map(Line::names_blobs);
        for hash in named {
            if !valid_blob_hash(hash) || !storage::exists(&live.blobs, hash) {
                return Err(AppendError::InvalidTransition);
            }
        }
        Ok(())
    }

    fn write_lines(&mut self, lines: &[Line<'_>], ts_ms: u64) -> Result<(), AppendError> {
        #[cfg(test)]
        if let Some(fault) = self.fail_next_write.take() {
            return Err(self.fail(fault));
        }
        let Phase::Live(live) = &mut self.phase else {
            return Err(AppendError::SessionClosed);
        };
        let first_seq = live.next_seq;
        let base = live.end;
        let mut batch = Vec::new();
        let mut starts = Vec::with_capacity(lines.len());
        for (seq, line) in (first_seq..).zip(lines) {
            starts.push(base + batch.len() as u64);
            append_line(&mut batch, seq, ts_ms, line).map_err(|_| AppendError::TooLarge)?;
        }
        if let Err(error) = live.log.write_all_at(&batch, live.end) {
            return Err(self.fail(io_fault(error)));
        }
        live.end += batch.len() as u64;
        live.next_seq += lines.len() as u64;
        for ((seq, line), offset) in (first_seq..).zip(lines).zip(starts) {
            self.state.apply(0, seq, offset, &line.body());
        }
        self.wrote(ts_ms);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), AppendError> {
        let Phase::Live(live) = &self.phase else {
            return Err(AppendError::SessionClosed);
        };
        match live.log.sync_all() {
            Ok(()) => Ok(()),
            Err(error) => Err(self.fail(io_fault(error))),
        }
    }

    fn fail(&mut self, fault: IoFault) -> AppendError {
        if matches!(self.phase, Phase::Live(_))
            && let Phase::Live(live) = mem::replace(&mut self.phase, Phase::Closed)
        {
            self.phase = Phase::Failed(Some(live));
        }
        self.fault.get_or_insert(fault);
        fault.into()
    }

    fn wrote(&mut self, ts_ms: u64) {
        self.updated_ms = ts_ms.max(self.created_ms.unwrap_or(0));
    }

    pub(crate) fn put_blob(&mut self, bytes: &[u8]) -> Result<String, AppendError> {
        if bytes.len() > MAX_BLOB_BYTES {
            return Err(AppendError::TooLarge);
        }
        let blobs = match &self.phase {
            Phase::Live(live) => &live.blobs,
            Phase::Held => return Err(AppendError::InvalidTransition),
            Phase::Failed(_) => return Err(self.fault.unwrap_or(IoFault::Io).into()),
            Phase::Closed => return Err(AppendError::SessionClosed),
        };
        let hash = lowercase_hex(&Sha256::digest(bytes));
        if storage::exists(blobs, &hash) {
            return Ok(hash);
        }
        let mut suffix = [0_u8; PENDING_SUFFIX_BYTES];
        getrandom::fill(&mut suffix).map_err(|_| IoFault::Io)?;
        let pending = format!(".pending.{}.tmp", lowercase_hex(&suffix));
        let stored = store_blob(blobs, &pending, &hash, bytes);
        if stored.is_err() {
            storage::remove(blobs, &pending);
        }
        stored?;
        Ok(hash)
    }

    pub(crate) fn close(&mut self) -> Result<(), AppendError> {
        match mem::replace(&mut self.phase, Phase::Closed) {
            Phase::Closed | Phase::Failed(None) => Ok(()),
            Phase::Held => {
                if self.held_lines > 0 {
                    diagnose("held_lines_dropped", &self.identity.id, self.held_lines);
                }
                self.held = Vec::new();
                self.held_lines = 0;
                Ok(())
            }
            Phase::Failed(Some(live)) => {
                release(&live);
                Ok(())
            }
            Phase::Live(live) => {
                self.phase = Phase::Live(live);
                let mut lines = Vec::with_capacity(2);
                if let Some(turn) = self.state.open_turn() {
                    lines.push(Line::TurnInterrupted(turn, Reason::Closed));
                }
                lines.push(Line::Closed);
                let closed = self
                    .write_lines(&lines, now_ms())
                    .and_then(|()| self.sync());
                if let Phase::Live(live) | Phase::Failed(Some(live)) =
                    mem::replace(&mut self.phase, Phase::Closed)
                {
                    self.update_index_in(&live.root, &Listing::Changed);
                    release(&live);
                }
                closed
            }
        }
    }

    fn update_index(&self, why: &Listing) {
        if let Phase::Live(live) | Phase::Failed(Some(live)) = &self.phase {
            self.update_index_in(&live.root, why);
        }
    }

    fn update_index_in(&self, root: &OwnedFd, why: &Listing) {
        let summary = self.summary(why);
        if catalog::put(root, &summary).is_err() {
            diagnose("index_stale", &self.identity.id, 0);
        }
    }

    fn summary(&self, why: &Listing) -> Summary {
        let now = now_ms();
        let updated_ms = if self.updated_ms == 0 {
            now
        } else {
            self.updated_ms
        };
        let mut opened_ms = [0; Host::ALL.len()];
        if matches!(why, Listing::Published) {
            opened_ms[self.identity.host as usize] = updated_ms;
        }
        Summary {
            id: self.identity.id.clone(),
            role: self.identity.role,
            host: self.identity.host,
            workspace: self
                .state
                .setting(SetKey::Workspace)
                .and_then(full_string)
                .unwrap_or_else(|| self.identity.workspace.clone()),
            title: self.state.setting(SetKey::Title).map(<[u8]>::to_vec),
            language: self.state.setting(SetKey::Language).map(<[u8]>::to_vec),
            parent: None,
            created_ms: self.created_ms.filter(|&ms| ms != 0).unwrap_or(now),
            updated_ms,
            turns: self.state.turns(),
            opened_ms,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn release(live: &Live) {
    storage::unlock(&live.lock);
}

fn store_blob(blobs: &OwnedFd, pending: &str, hash: &str, bytes: &[u8]) -> Result<(), IoFault> {
    let file = storage::create_file(blobs, pending, Mode::RUSR)?;
    file.write_all_at(bytes, 0).map_err(io_fault)?;
    file.sync_all().map_err(io_fault)?;
    drop(file);
    storage::rename(blobs, pending, hash)?;
    storage::sync_dir(blobs)
}

fn diagnose(kind: &str, session: &str, count: u64) {
    trace_log!(
        SESSION,
        "event=sessions_v2_diagnostic kind={kind} session={session} count={count} offset=0"
    );
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn changes_listing(events: &[Event<'_>]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            Event::Set(SetKey::Title | SetKey::Workspace | SetKey::Language, _)
        )
    })
}

fn needs_sync(lines: &[Line<'_>]) -> bool {
    lines.iter().any(|line| {
        matches!(
            line,
            Line::TurnCommitted(_)
                | Line::TurnInterrupted(..)
                | Line::Set(SetKey::Permissions | SetKey::Usage | SetKey::MovedFiles, _)
        )
    })
}

fn check_events(events: &[Event<'_>]) -> Result<(), AppendError> {
    if events.is_empty() {
        return Err(AppendError::InvalidArgument);
    }
    for event in events {
        match *event {
            Event::TurnStarted | Event::TurnCommitted => {}
            Event::Item {
                item_type,
                data,
                blobs,
            } => {
                if !valid_item_type(item_type) || !blobs.iter().all(|hash| valid_blob_hash(hash)) {
                    return Err(AppendError::InvalidArgument);
                }
                check_json(data)?;
            }
            Event::TurnInterrupted(reason) => {
                if !matches!(reason, Reason::Cancel | Reason::Failed) {
                    return Err(AppendError::InvalidArgument);
                }
            }
            Event::Set(key, value) => match key {
                SetKey::Title | SetKey::Workspace | SetKey::ClientPrompt => check_string(value)?,
                SetKey::Language => {
                    check_string(value)?;
                    let tag = full_string(value).ok_or(AppendError::InvalidArgument)?;
                    if tag.is_empty() || tag.len() > MAX_LANGUAGE_BYTES {
                        return Err(AppendError::InvalidArgument);
                    }
                }
                _ => check_json(value)?,
            },
        }
    }
    Ok(())
}

fn check_json(raw: &[u8]) -> Result<(), AppendError> {
    if raw.len() > MAX_VALUE_BYTES {
        return Err(AppendError::TooLarge);
    }
    let spaced = |byte: Option<&u8>| byte.is_none_or(|byte| b" \t\n\r".contains(byte));
    if spaced(raw.first()) || spaced(raw.last()) || value_end(raw, 0) != Some(raw.len()) {
        return Err(AppendError::InvalidArgument);
    }
    Ok(())
}

fn check_string(raw: &[u8]) -> Result<(), AppendError> {
    check_json(raw)?;
    if raw.len() < 2 || raw[0] != b'"' {
        return Err(AppendError::InvalidArgument);
    }
    Ok(())
}

fn valid_item_type(name: &str) -> bool {
    (1..=MAX_ITEM_TYPE_BYTES).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[cfg(test)]
mod tests;
