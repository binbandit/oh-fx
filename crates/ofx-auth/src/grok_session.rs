use std::path::PathBuf;
use std::time::{Duration, Instant};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir, RemoveOutcome, parse_strict_json};
use ofx_contract::valid_credential_account_id;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::chatgpt_session::DeleteOutcome;
use crate::grok_oauth::GrokError;
use crate::secret::Secret;
use crate::session_presence;
#[cfg(test)]
use crate::session_presence::Presence;

const SCHEMA_VERSION: i64 = 1;
const MAX_AUTH_FILE_BYTES: usize = 64 * 1024;
const EXPIRY_SKEW_MS: i64 = 60 * 1000;
pub(crate) const AUTH_FILE_NAME: &str = "grok-auth.json";
const MUTATION_LOCK_FILE_NAME: &str = "grok-auth.lock";
pub(crate) const MUTATION_LOCK_WAIT: Duration = Duration::from_secs(2);
const MUTATION_LOCK_RETRY: Duration = Duration::from_millis(10);

pub(crate) fn valid_account_id(account_id: &str) -> bool {
    valid_credential_account_id(account_id)
}

pub(crate) fn refresh_deadline_ms(expires_at_ms: i64) -> i64 {
    expires_at_ms.saturating_sub(EXPIRY_SKEW_MS).max(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    pub(crate) access_token: Secret,
    pub(crate) refresh_token: Secret,
    pub(crate) expires_at_ms: i64,
    pub(crate) account_id: String,
}

impl Session {
    pub(crate) fn expired(&self, now_ms: i64) -> bool {
        refresh_deadline_ms(self.expires_at_ms) <= now_ms
    }
}

#[derive(Debug)]
pub(crate) struct Mutation {
    directory: PrivateDir,
    _lock: AdvisoryLock,
}

impl Mutation {
    pub(crate) fn require_writable(&self) -> Result<(), GrokError> {
        session_presence::require_writable_in_dir(&self.directory, AUTH_FILE_NAME)
            .map(|_| ())
            .map_err(|_| GrokError::CredentialStorageUnavailable)
    }

    pub(crate) fn load(&self) -> Result<Option<Session>, GrokError> {
        load_from_dir(&self.directory)
    }

    pub(crate) fn save(&self, session: &Session) -> Result<(), GrokError> {
        self.directory
            .replace(AUTH_FILE_NAME, stringify(session)?.as_bytes())
            .map_err(durable_error)
    }

    pub(crate) fn delete(&self) -> Result<DeleteOutcome, GrokError> {
        match self.directory.remove(AUTH_FILE_NAME) {
            Ok(RemoveOutcome::Removed) => Ok(DeleteOutcome::Deleted),
            Ok(RemoveOutcome::Missing) => Ok(DeleteOutcome::Missing),
            Ok(RemoveOutcome::RemovedNotDurable) => Ok(DeleteOutcome::DeletedNotDurable),
            Err(error) => Err(storage_error(error)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionStore {
    directory: PathBuf,
}

impl SessionStore {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub(crate) fn load(&self) -> Result<Option<Session>, GrokError> {
        let Some(directory) = PrivateDir::open_existing(&self.directory).map_err(storage_error)?
        else {
            return Ok(None);
        };
        load_from_dir(&directory)
    }

    #[cfg(test)]
    pub(crate) fn presence(&self) -> Presence {
        session_presence::profile_file(&self.directory, AUTH_FILE_NAME, MAX_AUTH_FILE_BYTES)
    }

    pub(crate) fn require_sign_in_storage(&self) -> Result<(), GrokError> {
        session_presence::require_writable_profile_file(
            &self.directory,
            AUTH_FILE_NAME,
            MUTATION_LOCK_FILE_NAME,
        )
        .map(|_| ())
        .map_err(|_| GrokError::CredentialStorageUnavailable)
    }

    pub(crate) async fn save_new_session(&self, session: &Session) -> Result<(), GrokError> {
        let mutation = self.begin_mutation().await?;
        mutation.save(session)
    }

    pub(crate) async fn begin_existing_mutation(&self) -> Result<Option<Mutation>, GrokError> {
        match PrivateDir::open_existing_private(&self.directory) {
            Ok(Some(directory)) => lock_mutation(directory).await.map(Some),
            Ok(None) => Ok(None),
            Err(error) => Err(storage_error(error)),
        }
    }

    async fn begin_mutation(&self) -> Result<Mutation, GrokError> {
        let directory = PrivateDir::open_or_create(&self.directory).map_err(durable_error)?;
        lock_mutation(directory).await
    }
}

async fn lock_mutation(directory: PrivateDir) -> Result<Mutation, GrokError> {
    let started = Instant::now();
    loop {
        if let Some(lock) = directory
            .try_lock(MUTATION_LOCK_FILE_NAME)
            .map_err(storage_error)?
        {
            return Ok(Mutation {
                directory,
                _lock: lock,
            });
        }
        if started.elapsed() >= MUTATION_LOCK_WAIT {
            return Err(GrokError::LockBusy);
        }
        tokio::time::sleep(MUTATION_LOCK_RETRY).await;
    }
}

fn load_from_dir(directory: &PrivateDir) -> Result<Option<Session>, GrokError> {
    let bytes = match directory.read_private(AUTH_FILE_NAME, MAX_AUTH_FILE_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(DurableError::InsecureFile) => return Err(GrokError::InsecureAuthFile),
        Err(error) => return Err(storage_error(error)),
    };
    parse(&bytes).map(Some)
}

fn storage_error(_: DurableError) -> GrokError {
    GrokError::CredentialStorageUnavailable
}

fn durable_error(error: DurableError) -> GrokError {
    match error {
        DurableError::PathUnsafe => GrokError::DurablePathUnsafe,
        DurableError::PermissionsUnsupported => GrokError::PrivateStatePermissionsUnsupported,
        DurableError::PreRenameFailed => GrokError::DurableReplacePreRenameFailed,
        DurableError::PostRenameFailed => GrokError::DurableReplacePostRenameFailed,
        DurableError::AccessDenied => GrokError::AccessDenied,
        other => storage_error(other),
    }
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Session, GrokError> {
    let invalid = GrokError::InvalidGrokAuthSession;
    let Ok(Value::Object(object)) = parse_strict_json(bytes) else {
        return Err(invalid);
    };
    if object.get("version").and_then(Value::as_i64) != Some(SCHEMA_VERSION)
        || !object.get("version").is_some_and(Value::is_i64)
    {
        return Err(invalid);
    }
    let access_token = required_string(&object, "access_token")?;
    let refresh_token = required_string(&object, "refresh_token")?;
    let account_id = required_string(&object, "account_id")?.into_inner();
    if !valid_account_id(&account_id) {
        return Err(invalid);
    }
    let expires_at_ms = object
        .get("expires_at_ms")
        .filter(|value| value.is_i64())
        .and_then(Value::as_i64)
        .ok_or(invalid)?;
    Ok(Session {
        access_token,
        refresh_token,
        expires_at_ms,
        account_id,
    })
}

pub(crate) fn stringify(session: &Session) -> Result<Zeroizing<String>, GrokError> {
    if !valid_account_id(&session.account_id) {
        return Err(GrokError::InvalidGrokAuthSession);
    }
    Ok(Zeroizing::new(format!(
        "{{\"version\":{SCHEMA_VERSION},\"access_token\":{},\"refresh_token\":{},\"expires_at_ms\":{},\"account_id\":{}}}\n",
        *json_string(session.access_token.expose()),
        *json_string(session.refresh_token.expose()),
        session.expires_at_ms,
        *json_string(&session.account_id),
    )))
}

fn json_string(text: &str) -> Zeroizing<String> {
    Zeroizing::new(Value::String(text.to_owned()).to_string())
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<Secret, GrokError> {
    match object.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(Secret::new(value.clone())),
        _ => Err(GrokError::InvalidGrokAuthSession),
    }
}

#[cfg(test)]
mod tests;
