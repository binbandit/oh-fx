use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ofx_config::{DurableError, PrivateDir};
use serde::{Deserialize, Serialize};

use crate::terminal_contracts::{Dimensions, Lifecycle, LifecycleEvent};
use crate::terminal_recovery::{Disposition, reconcile};

const RECORD_SCHEMA_VERSION: u16 = 2;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_SESSION_ID_BYTES: usize = 255;
const SESSIONS_V2_DIRECTORY: &str = "v2";
const TERMINAL_ID_PREFIX: &str = "shell-";
const TERMINAL_ID_RANDOM_BYTES: usize = 16;
const OWNER_IDENTITY_PREFIX: &str = "oh-fx-process-v1/";
const OWNER_INSTANCE_BYTES: usize = 16;
const RECORD_PREFIX: &str = "record-";
const RECORD_SUFFIX: &str = ".json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("InvalidSessionId")]
    InvalidSessionId,
    #[error("InvalidTerminalRecord")]
    InvalidRecord,
    #[error("TerminalRecordNotFound")]
    RecordNotFound,
    #[error("TerminalRecordTooLarge")]
    RecordTooLarge,
    #[error("InvalidLifecycle")]
    InvalidLifecycle,
    #[error("CapacityExceeded")]
    CapacityExceeded,
    #[error("SystemResources")]
    SystemResources,
    #[error("{0}")]
    Durable(#[from] DurableError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Termination {
    Exited(i32),
    Signal(u32),
}

impl Termination {
    fn is_valid(self) -> bool {
        match self {
            Self::Exited(code) => (0..=255).contains(&code),
            Self::Signal(signal) => (1..=255).contains(&signal),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalRecord {
    pub schema_version: u16,
    pub session_id: String,
    pub owner_session_id: String,
    pub host_identity: String,
    pub shell: String,
    pub cwd: String,
    pub command: Option<String>,
    pub timeout_ms: Option<u64>,
    pub timeout_at_ms: Option<i64>,
    pub timed_out: bool,
    pub lifecycle: Lifecycle,
    pub dimensions: Dimensions,
    pub pid: Option<String>,
    pub termination: Option<Termination>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl TerminalRecord {
    fn is_valid(&self) -> bool {
        let timeout_consistent = match (self.timeout_ms, self.timeout_at_ms) {
            (_, None) => !self.timed_out,
            (Some(_), Some(deadline)) => deadline >= self.created_at_ms,
            (None, Some(_)) => false,
        };
        self.schema_version == RECORD_SCHEMA_VERSION
            && is_valid_session_id(&self.session_id)
            && is_valid_session_id(&self.owner_session_id)
            && !self.host_identity.is_empty()
            && !self.shell.is_empty()
            && Path::new(&self.cwd).is_absolute()
            && timeout_consistent
            && self.dimensions.is_valid()
            && self.termination.is_none_or(Termination::is_valid)
    }

    fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        serde_json::from_slice::<Self>(bytes)
            .ok()
            .filter(Self::is_valid)
            .ok_or(StoreError::InvalidRecord)
    }

    fn encode(&self) -> Result<Vec<u8>, StoreError> {
        if !self.is_valid() {
            return Err(StoreError::InvalidRecord);
        }
        let bytes = serde_json::to_vec(self).map_err(|_| StoreError::InvalidRecord)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(StoreError::RecordTooLarge);
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NewTerminal<'a> {
    pub owner_session_id: &'a str,
    pub shell: &'a str,
    pub cwd: &'a str,
    pub command: &'a str,
    pub timeout_ms: Option<u64>,
    pub dimensions: Dimensions,
}

#[derive(Debug)]
pub struct TerminalStore {
    root: PathBuf,
    host_identity: String,
}

impl TerminalStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let mut instance = [0_u8; OWNER_INSTANCE_BYTES];
        getrandom::fill(&mut instance).map_err(|_| StoreError::SystemResources)?;
        let mut host_identity = String::from(OWNER_IDENTITY_PREFIX);
        for byte in instance {
            let _ = write!(host_identity, "{byte:02x}");
        }
        let _ = write!(host_identity, "/{}", std::process::id());
        Ok(Self {
            root: root.into(),
            host_identity,
        })
    }

    pub fn create(
        &self,
        terminal: &NewTerminal<'_>,
        now_ms: i64,
    ) -> Result<DurableTerminal, StoreError> {
        if !is_valid_session_id(terminal.owner_session_id) {
            return Err(StoreError::InvalidSessionId);
        }
        let record = TerminalRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            session_id: generate_terminal_id()?,
            owner_session_id: terminal.owner_session_id.to_owned(),
            host_identity: self.host_identity.clone(),
            shell: terminal.shell.to_owned(),
            cwd: terminal.cwd.to_owned(),
            command: Some(terminal.command.to_owned()),
            timeout_ms: terminal.timeout_ms,
            timeout_at_ms: None,
            timed_out: false,
            lifecycle: Lifecycle::Starting,
            dimensions: terminal.dimensions,
            pid: None,
            termination: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let directory = PrivateDir::open_or_create(&self.root)?
            .open_or_create_child(terminal.owner_session_id)?;
        save(&directory, &record)?;
        Ok(DurableTerminal { directory, record })
    }

    pub fn open(
        &self,
        owner_session_id: &str,
        session_id: &str,
    ) -> Result<DurableTerminal, StoreError> {
        if !is_valid_session_id(session_id) {
            return Err(StoreError::InvalidSessionId);
        }
        let directory = self
            .owner(owner_session_id)?
            .ok_or(StoreError::RecordNotFound)?;
        let record = load(&directory, session_id)?;
        if record.owner_session_id != owner_session_id {
            return Err(StoreError::InvalidRecord);
        }
        Ok(DurableTerminal { directory, record })
    }

    pub fn reconcile_owner(
        &self,
        owner_session_id: &str,
        now_ms: i64,
    ) -> Result<Vec<TerminalRecord>, StoreError> {
        let Some(directory) = self.owner(owner_session_id)? else {
            return Ok(Vec::new());
        };
        let mut records = Vec::new();
        for name in directory.names()? {
            let Some(session_id) = terminal_id_from_record_name(&name) else {
                continue;
            };
            let Ok(mut record) = load(&directory, session_id) else {
                continue;
            };
            if record.owner_session_id != owner_session_id {
                continue;
            }
            let decision = reconcile(
                record.lifecycle,
                record.termination.is_some(),
                record.host_identity == self.host_identity,
            );
            if matches!(
                decision.disposition,
                Disposition::FinalizeExited | Disposition::MarkLost
            ) {
                record.lifecycle = decision.lifecycle;
                record.updated_at_ms = now_ms;
                save(&directory, &record)?;
            }
            records.push(record);
        }
        records.sort_by(|left, right| {
            (left.created_at_ms, &left.session_id).cmp(&(right.created_at_ms, &right.session_id))
        });
        Ok(records)
    }

    fn owner(&self, owner_session_id: &str) -> Result<Option<PrivateDir>, StoreError> {
        if !is_valid_session_id(owner_session_id) {
            return Err(StoreError::InvalidSessionId);
        }
        let Some(root) = PrivateDir::open_existing_private(&self.root)? else {
            return Ok(None);
        };
        Ok(root.open_child_private(owner_session_id)?)
    }
}

#[derive(Debug)]
pub struct DurableTerminal {
    directory: PrivateDir,
    record: TerminalRecord,
}

impl DurableTerminal {
    #[must_use]
    pub fn record(&self) -> &TerminalRecord {
        &self.record
    }

    pub fn mark_started(&mut self, pid: u32, now_ms: i64) -> Result<(), StoreError> {
        self.update(now_ms, |record| {
            if record.lifecycle != Lifecycle::Starting {
                return Err(StoreError::InvalidLifecycle);
            }
            record.timeout_at_ms = record
                .timeout_ms
                .map(|timeout| {
                    i64::try_from(timeout)
                        .ok()
                        .and_then(|timeout| now_ms.checked_add(timeout))
                        .ok_or(StoreError::CapacityExceeded)
                })
                .transpose()?;
            record.pid = Some(pid.to_string());
            record.lifecycle = transition(record.lifecycle, LifecycleEvent::ChildStarted)?;
            Ok(())
        })
    }

    pub fn mark_timed_out(&mut self, now_ms: i64) -> Result<(), StoreError> {
        if self.record.timed_out {
            return Ok(());
        }
        self.update(now_ms, |record| {
            if record.timeout_at_ms.is_none() {
                return Err(StoreError::InvalidLifecycle);
            }
            record.timed_out = true;
            Ok(())
        })
    }

    pub fn persist_termination(
        &mut self,
        termination: Termination,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.update(now_ms, |record| {
            if record.lifecycle == Lifecycle::Closed || !termination.is_valid() {
                return Err(StoreError::InvalidLifecycle);
            }
            if record.lifecycle.is_live() {
                record.lifecycle = transition(record.lifecycle, LifecycleEvent::ChildExited)?;
            }
            record.termination = Some(termination);
            Ok(())
        })
    }

    #[must_use]
    pub fn termination(&self) -> Option<Termination> {
        self.record.termination
    }

    pub fn persist_lost(&mut self, now_ms: i64) -> Result<(), StoreError> {
        self.update(now_ms, |record| {
            if record.lifecycle == Lifecycle::Closed {
                return Err(StoreError::InvalidLifecycle);
            }
            if record.lifecycle.is_live() {
                record.lifecycle = transition(record.lifecycle, LifecycleEvent::HostLost)?;
            }
            Ok(())
        })
    }

    pub fn close(&mut self, now_ms: i64) -> Result<(), StoreError> {
        self.update(now_ms, |record| {
            record.lifecycle = transition(record.lifecycle, LifecycleEvent::Close)?;
            Ok(())
        })
    }

    fn update(
        &mut self,
        now_ms: i64,
        change: impl FnOnce(&mut TerminalRecord) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut next = self.record.clone();
        change(&mut next)?;
        next.updated_at_ms = now_ms;
        save(&self.directory, &next)?;
        self.record = next;
        Ok(())
    }
}

fn transition(lifecycle: Lifecycle, event: LifecycleEvent) -> Result<Lifecycle, StoreError> {
    lifecycle
        .transition(event)
        .map_err(|_| StoreError::InvalidLifecycle)
}

fn save(directory: &PrivateDir, record: &TerminalRecord) -> Result<(), StoreError> {
    let bytes = record.encode()?;
    directory.replace(&record_name(&record.session_id), &bytes)?;
    Ok(())
}

fn load(directory: &PrivateDir, session_id: &str) -> Result<TerminalRecord, StoreError> {
    let bytes = match directory.read_private(&record_name(session_id), MAX_RECORD_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Err(StoreError::RecordNotFound),
        Err(DurableError::TooLarge) => return Err(StoreError::RecordTooLarge),
        Err(error) => return Err(error.into()),
    };
    let record = TerminalRecord::decode(&bytes)?;
    if record.session_id != session_id {
        return Err(StoreError::InvalidRecord);
    }
    Ok(record)
}

fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_BYTES
        && id != "."
        && id != ".."
        && !id.eq_ignore_ascii_case(SESSIONS_V2_DIRECTORY)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn generate_terminal_id() -> Result<String, StoreError> {
    let mut random = [0_u8; TERMINAL_ID_RANDOM_BYTES];
    getrandom::fill(&mut random).map_err(|_| StoreError::SystemResources)?;
    Ok(format!(
        "{TERMINAL_ID_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(random)
    ))
}

fn record_name(session_id: &str) -> String {
    format!("{RECORD_PREFIX}{session_id}{RECORD_SUFFIX}")
}

fn terminal_id_from_record_name(name: &str) -> Option<&str> {
    name.strip_prefix(RECORD_PREFIX)?
        .strip_suffix(RECORD_SUFFIX)
        .filter(|id| is_valid_session_id(id))
}

#[cfg(test)]
mod tests;
