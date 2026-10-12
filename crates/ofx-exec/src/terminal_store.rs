use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ofx_config::{DurableError, PrivateDir};
use ofx_contract::valid_session_id;
use serde::{Deserialize, Serialize};

use crate::terminal_contracts::{
    Dimensions, Lifecycle, LifecycleEvent, ReturnOutcome, StructuredErrorCode,
};
use crate::terminal_recovery::{Disposition, reconcile};

const RECORD_SCHEMA_VERSION: u16 = 2;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const TERMINAL_ID_PREFIX: &str = "shell-";
const TERMINAL_ID_RANDOM_BYTES: usize = 16;
const OWNER_IDENTITY_PREFIX: &str = "oh-fx-process-v1/";
const OWNER_INSTANCE_BYTES: usize = 16;
const RECORD_PREFIX: &str = "record-";
const RECORD_SUFFIX: &str = ".json";

static HOST_IDENTITY: OnceLock<Option<String>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("InvalidSessionId")]
    InvalidSessionId,
    #[error("InvalidTerminalRecord")]
    InvalidRecord,
    #[error("UnsupportedTerminalSchema")]
    UnsupportedSchema,
    #[error("TerminalOwnedByAnotherSession")]
    OwnedByAnotherSession,
    #[error("DuplicateTerminalSession")]
    DuplicateTerminalSession,
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
pub(crate) enum Termination {
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

impl StoreError {
    pub(crate) fn code(self) -> StructuredErrorCode {
        match self {
            Self::OwnedByAnotherSession => StructuredErrorCode::AuthorityDenied,
            Self::InvalidLifecycle => StructuredErrorCode::InvalidLifecycle,
            Self::CapacityExceeded => StructuredErrorCode::CapacityExceeded,
            _ => StructuredErrorCode::InvalidRequest,
        }
    }
}

impl From<Termination> for ReturnOutcome {
    fn from(termination: Termination) -> Self {
        match termination {
            Termination::Exited(code) => Self::Exited(code),
            Termination::Signal(signal) => Self::Signal(signal),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalRecord {
    pub(crate) schema_version: u16,
    pub(crate) session_id: String,
    pub(crate) owner_session_id: String,
    pub(crate) host_identity: String,
    pub(crate) shell: String,
    pub(crate) cwd: String,
    pub(crate) command: Option<String>,
    #[serde(default)]
    pub(crate) timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) timeout_at_ms: Option<i64>,
    #[serde(default)]
    pub(crate) timed_out: bool,
    pub(crate) lifecycle: Lifecycle,
    pub(crate) dimensions: Dimensions,
    pub(crate) pid: Option<String>,
    pub(crate) termination: Option<Termination>,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
}

impl TerminalRecord {
    fn is_valid(&self) -> bool {
        let timeout_consistent = match (self.timeout_ms, self.timeout_at_ms) {
            (_, None) => !self.timed_out,
            (Some(_), Some(deadline)) => deadline >= self.created_at_ms,
            (None, Some(_)) => false,
        };
        self.schema_version == RECORD_SCHEMA_VERSION
            && valid_session_id(&self.session_id)
            && valid_session_id(&self.owner_session_id)
            && !self.host_identity.is_empty()
            && !self.shell.is_empty()
            && Path::new(&self.cwd).is_absolute()
            && timeout_consistent
            && self.dimensions.is_valid()
            && self.termination.is_none_or(Termination::is_valid)
    }

    fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        let version: SchemaVersion =
            serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidRecord)?;
        if version.schema_version != RECORD_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema);
        }
        serde_json::from_slice::<Self>(bytes)
            .ok()
            .filter(Self::is_valid)
            .ok_or(StoreError::InvalidRecord)
    }

    fn encode(&self) -> Result<Vec<u8>, StoreError> {
        if !self.is_valid() {
            return Err(StoreError::InvalidRecord);
        }
        let mut bytes = serde_json::to_vec(self).map_err(|_| StoreError::InvalidRecord)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(StoreError::RecordTooLarge);
        }
        Ok(bytes)
    }
}

#[derive(Deserialize)]
struct SchemaVersion {
    schema_version: u16,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NewTerminal<'a> {
    pub(crate) owner_session_id: &'a str,
    pub(crate) shell: &'a str,
    pub(crate) cwd: &'a str,
    pub(crate) command: &'a str,
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) dimensions: Dimensions,
}

#[derive(Debug)]
pub struct TerminalStore {
    root: PathBuf,
    host_identity: &'static str,
}

impl TerminalStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        Ok(Self {
            root: root.into(),
            host_identity: host_identity()?,
        })
    }

    pub(crate) fn create(
        &self,
        terminal: &NewTerminal<'_>,
        now_ms: i64,
    ) -> Result<DurableTerminal, StoreError> {
        if !valid_session_id(terminal.owner_session_id) {
            return Err(StoreError::InvalidSessionId);
        }
        let record = TerminalRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            session_id: generate_terminal_id()?,
            owner_session_id: terminal.owner_session_id.to_owned(),
            host_identity: self.host_identity.to_owned(),
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
        let bytes = record.encode()?;
        let directory = PrivateDir::open_or_create(&self.root)?
            .open_or_create_child(terminal.owner_session_id)?;
        directory.replace(&record_name(&record.session_id), &bytes)?;
        Ok(DurableTerminal { directory, record })
    }

    pub(crate) fn open(
        &self,
        owner_session_id: &str,
        session_id: &str,
    ) -> Result<DurableTerminal, StoreError> {
        if !valid_session_id(owner_session_id) || !valid_session_id(session_id) {
            return Err(StoreError::InvalidSessionId);
        }
        let owners = self.owners_of(session_id)?;
        match owners.as_slice() {
            [] => Err(StoreError::RecordNotFound),
            [owner] if owner == owner_session_id => {
                let directory = self
                    .owner(owner_session_id)?
                    .ok_or(StoreError::RecordNotFound)?;
                let record = load(&directory, session_id)?;
                if record.owner_session_id != owner_session_id {
                    return Err(StoreError::InvalidRecord);
                }
                Ok(DurableTerminal { directory, record })
            }
            [_] => Err(StoreError::OwnedByAnotherSession),
            _ => Err(StoreError::DuplicateTerminalSession),
        }
    }

    pub(crate) fn reconcile_owner(
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
            if record.host_identity == self.host_identity {
                records.push(record);
                continue;
            }
            let decision = reconcile(record.lifecycle, record.termination.is_some(), false);
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

    fn owners_of(&self, session_id: &str) -> Result<Vec<String>, StoreError> {
        let Some(root) = PrivateDir::open_existing_private(&self.root)? else {
            return Ok(Vec::new());
        };
        let name = record_name(session_id);
        let mut owners = Vec::new();
        for owner in root.names()? {
            if !valid_session_id(&owner) {
                continue;
            }
            let held = root
                .open_child(&owner)
                .ok()
                .flatten()
                .is_some_and(|directory| directory.private_file_exists(&name).unwrap_or(true));
            if held {
                owners.push(owner);
            }
        }
        Ok(owners)
    }

    fn owner(&self, owner_session_id: &str) -> Result<Option<PrivateDir>, StoreError> {
        if !valid_session_id(owner_session_id) {
            return Err(StoreError::InvalidSessionId);
        }
        let Some(root) = PrivateDir::open_existing_private(&self.root)? else {
            return Ok(None);
        };
        Ok(root.open_child_private(owner_session_id)?)
    }
}

#[derive(Debug)]
pub(crate) struct DurableTerminal {
    directory: PrivateDir,
    record: TerminalRecord,
}

impl DurableTerminal {
    #[must_use]
    pub(crate) fn record(&self) -> &TerminalRecord {
        &self.record
    }

    pub(crate) fn mark_started(&mut self, pid: u32, now_ms: i64) -> Result<(), StoreError> {
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

    pub(crate) fn mark_timed_out(&mut self, now_ms: i64) -> Result<(), StoreError> {
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

    pub(crate) fn persist_termination(
        &mut self,
        termination: Termination,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        if !termination.is_valid() {
            return Err(StoreError::InvalidRecord);
        }
        self.update(now_ms, |record| {
            if record.lifecycle.is_live() {
                record.lifecycle = transition(record.lifecycle, LifecycleEvent::ChildExited)?;
            }
            record.termination = Some(termination);
            Ok(())
        })
    }

    #[must_use]
    pub(crate) fn termination(&self) -> Option<Termination> {
        self.record.termination
    }

    pub(crate) fn persist_lost(&mut self, now_ms: i64) -> Result<(), StoreError> {
        self.update(now_ms, |record| {
            if record.lifecycle.is_live() {
                record.lifecycle = transition(record.lifecycle, LifecycleEvent::HostLost)?;
            }
            Ok(())
        })
    }

    pub(crate) fn close(&mut self, now_ms: i64) -> Result<(), StoreError> {
        self.update(now_ms, |record| {
            record.lifecycle = transition(record.lifecycle, LifecycleEvent::Close)?;
            Ok(())
        })
    }

    pub(crate) fn rollback_unreleased_start(&self) -> Result<(), StoreError> {
        if self.record.lifecycle != Lifecycle::Starting || self.record.pid.is_some() {
            return Err(StoreError::InvalidLifecycle);
        }
        self.directory
            .remove(&record_name(&self.record.session_id))?;
        Ok(())
    }

    fn update(
        &mut self,
        now_ms: i64,
        change: impl FnOnce(&mut TerminalRecord) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut next = self.record.clone();
        change(&mut next)?;
        next.updated_at_ms = now_ms;
        let saved = save(&self.directory, &next);
        if matches!(
            saved,
            Ok(()) | Err(StoreError::Durable(DurableError::PostRenameFailed))
        ) {
            self.record = next;
        }
        saved
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
    #[cfg(test)]
    tests::fail_after_rename()?;
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

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

fn host_identity() -> Result<&'static str, StoreError> {
    HOST_IDENTITY
        .get_or_init(|| {
            let mut instance = [0_u8; OWNER_INSTANCE_BYTES];
            getrandom::fill(&mut instance).ok()?;
            let mut identity = String::from(OWNER_IDENTITY_PREFIX);
            for byte in instance {
                let _ = write!(identity, "{byte:02x}");
            }
            let _ = write!(identity, "/{}", std::process::id());
            Some(identity)
        })
        .as_deref()
        .ok_or(StoreError::SystemResources)
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
        .filter(|id| valid_session_id(id))
}

#[cfg(test)]
mod tests;
