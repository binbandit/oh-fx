use std::path::PathBuf;
use std::time::{Duration, Instant};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir, RemoveOutcome, parse_strict_json};
use ofx_contract::valid_credential_account_id;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::secret::Secret;
use crate::session_presence::{self, Presence};
use crate::subscription_access::{SubscriptionAccess, refresh_deadline_ms};
use std::marker::PhantomData;

const SCHEMA_VERSION: i64 = 1;
const MAX_AUTH_FILE_BYTES: usize = 64 * 1024;
pub(crate) const MUTATION_LOCK_WAIT: Duration = Duration::from_secs(2);
const MUTATION_LOCK_RETRY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionError {
    CredentialStorageUnavailable,
    InsecureAuthFile,
    InvalidSession,
    LockBusy,
    DurablePathUnsafe,
    PrivateStatePermissionsUnsupported,
    DurableReplacePreRenameFailed,
    DurableReplacePostRenameFailed,
    AccessDenied,
    Cancelled,
    CredentialRefreshPersistenceUncertain,
}

pub(crate) trait SessionPolicy {
    type Error;
    const AUTH_FILE_NAME: &'static str;
    const LOCK_FILE_NAME: &'static str;
    const VALIDATE_ACCOUNT_ON_WRITE: bool;
    fn error(error: SessionError) -> Self::Error;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    pub(crate) access_token: Secret,
    pub(crate) refresh_token: Secret,
    pub(crate) expires_at_ms: i64,
    pub(crate) account_id: String,
}

impl Session {
    pub(crate) fn into_access(self) -> SubscriptionAccess {
        SubscriptionAccess {
            refresh_after_ms: refresh_deadline_ms(self.expires_at_ms),
            access_token: self.access_token,
            account_id: self.account_id,
        }
    }

    pub(crate) fn expired(&self, now_ms: i64) -> bool {
        refresh_deadline_ms(self.expires_at_ms) <= now_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    Missing,
    DeletedNotDurable,
}

#[derive(Debug)]
pub(crate) struct Mutation<P> {
    policy: PhantomData<P>,
    directory: PrivateDir,
    _lock: AdvisoryLock,
}

impl<P: SessionPolicy> Mutation<P> {
    pub(crate) fn require_writable(&self) -> Result<(), P::Error> {
        session_presence::require_writable_in_dir(&self.directory, P::AUTH_FILE_NAME)
            .map(|_| ())
            .map_err(|_| P::error(SessionError::CredentialStorageUnavailable))
    }

    pub(crate) fn load(&self) -> Result<Option<Session>, P::Error> {
        load_from_dir::<P>(&self.directory)
    }

    pub(crate) fn save(&self, session: &Session) -> Result<(), P::Error> {
        self.directory
            .replace(P::AUTH_FILE_NAME, encode::<P>(session)?.as_bytes())
            .map_err(durable_error::<P>)
    }

    pub(crate) fn retire(&self) -> Result<(), P::Error> {
        match self.delete() {
            Ok(DeleteOutcome::Deleted | DeleteOutcome::Missing) => Ok(()),
            _ => Err(P::error(
                SessionError::CredentialRefreshPersistenceUncertain,
            )),
        }
    }

    pub(crate) fn delete(&self) -> Result<DeleteOutcome, P::Error> {
        match self.directory.remove(P::AUTH_FILE_NAME) {
            Ok(RemoveOutcome::Removed) => Ok(DeleteOutcome::Deleted),
            Ok(RemoveOutcome::Missing) => Ok(DeleteOutcome::Missing),
            Ok(RemoveOutcome::RemovedNotDurable) => Ok(DeleteOutcome::DeletedNotDurable),
            Err(error) => Err(storage_error::<P>(error)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionStore<P> {
    policy: PhantomData<P>,
    directory: PathBuf,
}

impl<P: SessionPolicy> SessionStore<P> {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            policy: PhantomData,
        }
    }

    pub(crate) fn presence(&self) -> Presence {
        session_presence::profile_file(&self.directory, P::AUTH_FILE_NAME, MAX_AUTH_FILE_BYTES)
    }

    pub(crate) fn require_sign_in_storage(&self) -> Result<(), P::Error> {
        session_presence::require_writable_profile_file(
            &self.directory,
            P::AUTH_FILE_NAME,
            P::LOCK_FILE_NAME,
        )
        .map(|_| ())
        .map_err(|_| P::error(SessionError::CredentialStorageUnavailable))
    }

    pub(crate) async fn save_new_session(&self, session: &Session) -> Result<(), P::Error> {
        let mutation = self.begin_mutation().await?;
        mutation.save(session)
    }

    pub(crate) async fn save_new_session_cancellable(
        &self,
        session: &Session,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), P::Error> {
        let mutation = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(P::error(SessionError::Cancelled)),
            mutation = self.begin_mutation() => mutation?,
        };
        if cancel.is_cancelled() {
            return Err(P::error(SessionError::Cancelled));
        }
        mutation.save(session)
    }

    pub(crate) fn load_unlocked(&self) -> Result<Option<Session>, P::Error> {
        match PrivateDir::open_existing_private(&self.directory) {
            Ok(Some(directory)) => load_from_dir::<P>(&directory),
            Ok(None) => Ok(None),
            Err(error) => Err(storage_error::<P>(error)),
        }
    }

    pub(crate) async fn begin_existing_mutation(&self) -> Result<Option<Mutation<P>>, P::Error> {
        match PrivateDir::open_existing_private(&self.directory) {
            Ok(Some(directory)) => lock_mutation::<P>(directory).await.map(Some),
            Ok(None) => Ok(None),
            Err(error) => Err(storage_error::<P>(error)),
        }
    }

    pub(crate) async fn begin_mutation(&self) -> Result<Mutation<P>, P::Error> {
        let directory = PrivateDir::open_or_create(&self.directory).map_err(durable_error::<P>)?;
        lock_mutation::<P>(directory).await
    }
}

async fn lock_mutation<P: SessionPolicy>(directory: PrivateDir) -> Result<Mutation<P>, P::Error> {
    let started = Instant::now();
    loop {
        if let Some(lock) = directory
            .try_lock(P::LOCK_FILE_NAME)
            .map_err(storage_error::<P>)?
        {
            return Ok(Mutation {
                directory,
                policy: PhantomData,
                _lock: lock,
            });
        }
        if started.elapsed() >= MUTATION_LOCK_WAIT {
            return Err(P::error(SessionError::LockBusy));
        }
        tokio::time::sleep(MUTATION_LOCK_RETRY).await;
    }
}

fn load_from_dir<P: SessionPolicy>(directory: &PrivateDir) -> Result<Option<Session>, P::Error> {
    let bytes = match directory.read_private(P::AUTH_FILE_NAME, MAX_AUTH_FILE_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(DurableError::InsecureFile) => return Err(P::error(SessionError::InsecureAuthFile)),
        Err(error) => return Err(storage_error::<P>(error)),
    };
    parse(&bytes).map_err(P::error).map(Some)
}

fn storage_error<P: SessionPolicy>(_: DurableError) -> P::Error {
    P::error(SessionError::CredentialStorageUnavailable)
}

fn durable_error<P: SessionPolicy>(error: DurableError) -> P::Error {
    match error {
        DurableError::PathUnsafe => P::error(SessionError::DurablePathUnsafe),
        DurableError::PermissionsUnsupported => {
            P::error(SessionError::PrivateStatePermissionsUnsupported)
        }
        DurableError::PreRenameFailed => P::error(SessionError::DurableReplacePreRenameFailed),
        DurableError::PostRenameFailed => P::error(SessionError::DurableReplacePostRenameFailed),
        DurableError::AccessDenied => P::error(SessionError::AccessDenied),
        other => storage_error::<P>(other),
    }
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Session, SessionError> {
    let invalid = SessionError::InvalidSession;
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
    if !valid_credential_account_id(&account_id) {
        return Err(invalid);
    }
    let expires_at_ms = object
        .get("expires_at_ms")
        .filter(|value| value.is_i64())
        .and_then(Value::as_i64)
        .ok_or(SessionError::InvalidSession)?;
    Ok(Session {
        access_token,
        refresh_token,
        expires_at_ms,
        account_id,
    })
}

fn stringify(session: &Session) -> Zeroizing<String> {
    Zeroizing::new(format!(
        "{{\"version\":{SCHEMA_VERSION},\"access_token\":{},\"refresh_token\":{},\"expires_at_ms\":{},\"account_id\":{}}}\n",
        *json_string(session.access_token.expose()),
        *json_string(session.refresh_token.expose()),
        session.expires_at_ms,
        *json_string(&session.account_id),
    ))
}

fn json_string(text: &str) -> Zeroizing<String> {
    Zeroizing::new(Value::String(text.to_owned()).to_string())
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<Secret, SessionError> {
    match object.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(Secret::new(value.clone())),
        _ => Err(SessionError::InvalidSession),
    }
}

pub(crate) fn encode<P: SessionPolicy>(session: &Session) -> Result<Zeroizing<String>, P::Error> {
    if P::VALIDATE_ACCOUNT_ON_WRITE && !valid_credential_account_id(&session.account_id) {
        return Err(P::error(SessionError::InvalidSession));
    }
    Ok(stringify(session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatgpt_oauth::ChatGptError;
    use crate::chatgpt_session::ChatGptPolicy;
    use crate::grok_oauth::GrokError;
    use crate::grok_session::GrokPolicy;

    #[tokio::test]
    async fn provider_write_validation_remains_distinct() {
        let root = tempfile::tempdir().expect("temporary profile");
        let session = Session {
            access_token: Secret::new("access".to_owned()),
            refresh_token: Secret::new("refresh".to_owned()),
            expires_at_ms: 1,
            account_id: "invalid\naccount".to_owned(),
        };
        let chatgpt = SessionStore::<ChatGptPolicy>::new(root.path().join("chatgpt"));
        chatgpt
            .save_new_session(&session)
            .await
            .expect("existing ChatGPT writer permits account until load");
        assert_eq!(
            chatgpt.begin_mutation().await.unwrap().load(),
            Err(ChatGptError::InvalidChatGptAuthSession)
        );
        let grok = SessionStore::<GrokPolicy>::new(root.path().join("grok"));
        assert_eq!(
            grok.save_new_session(&session).await,
            Err(GrokError::InvalidGrokAuthSession)
        );
        assert_eq!(grok.begin_mutation().await.unwrap().load(), Ok(None));
    }
}
