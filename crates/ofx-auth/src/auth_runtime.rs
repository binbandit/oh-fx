use std::path::PathBuf;

use ofx_config::ProviderId;
use ofx_contract::valid_credential_account_id;
use ofx_trace::trace_log;
use tokio_util::sync::CancellationToken;

use crate::chatgpt_oauth::{ChatGptAccess, ChatGptError, ChatGptOAuth, RefreshMode};
use crate::chatgpt_session::SessionStore;
use crate::grok_session;
use crate::provider_catalog;
use crate::session_presence::Presence;
use crate::subscription_access::now_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialFailureReason {
    TemporaryUnavailable,
    InvalidCredential,
    InvalidStorage,
    PersistenceUncertain,
    AuthorityChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PreparationError {
    #[error("CredentialStorageUnavailable")]
    CredentialStorageUnavailable,
    #[error("CredentialTemporarilyUnavailable")]
    CredentialTemporarilyUnavailable,
    #[error("CredentialRefreshPersistenceUncertain")]
    CredentialRefreshPersistenceUncertain,
    #[error("CredentialAuthorityChanged")]
    CredentialAuthorityChanged,
}

impl PreparationError {
    pub const fn notice(self) -> &'static str {
        match self {
            Self::CredentialStorageUnavailable => {
                "Saved credential storage is unavailable. Check the saved credential, then retry."
            }
            Self::CredentialTemporarilyUnavailable => {
                "Credential refresh is temporarily unavailable. Retry shortly."
            }
            Self::CredentialRefreshPersistenceUncertain => {
                "Credential could not be saved. Check authentication storage before signing in again."
            }
            Self::CredentialAuthorityChanged => {
                "The credential account or team changed. Review authentication before retrying."
            }
        }
    }
}

fn classify_credential_failure(error: ChatGptError) -> CredentialFailureReason {
    match error {
        ChatGptError::AccessDenied | ChatGptError::CredentialRefreshRejected => {
            CredentialFailureReason::InvalidCredential
        }
        ChatGptError::CredentialStorageUnavailable
        | ChatGptError::DurablePathUnsafe
        | ChatGptError::InsecureAuthFile
        | ChatGptError::InvalidChatGptAuthSession
        | ChatGptError::PrivateStatePermissionsUnsupported => {
            CredentialFailureReason::InvalidStorage
        }
        ChatGptError::CredentialRefreshPersistenceUncertain
        | ChatGptError::CredentialPersistenceFailed
        | ChatGptError::DurableReplacePreRenameFailed
        | ChatGptError::DurableReplacePostRenameFailed => {
            CredentialFailureReason::PersistenceUncertain
        }
        ChatGptError::ChatGptAccountChanged => CredentialFailureReason::AuthorityChanged,
        _ => CredentialFailureReason::TemporaryUnavailable,
    }
}

fn classify_grok_credential_failure(
    error: crate::grok_oauth::GrokError,
) -> CredentialFailureReason {
    use crate::grok_oauth::GrokError;
    match error {
        GrokError::AccessDenied => CredentialFailureReason::InvalidCredential,
        GrokError::CredentialStorageUnavailable
        | GrokError::DurablePathUnsafe
        | GrokError::InsecureAuthFile
        | GrokError::InvalidGrokAuthSession
        | GrokError::PrivateStatePermissionsUnsupported => CredentialFailureReason::InvalidStorage,
        GrokError::CredentialRefreshPersistenceUncertain
        | GrokError::CredentialPersistenceFailed
        | GrokError::DurableReplacePreRenameFailed
        | GrokError::DurableReplacePostRenameFailed => {
            CredentialFailureReason::PersistenceUncertain
        }
        _ => CredentialFailureReason::TemporaryUnavailable,
    }
}

const AUTH: &str = "auth";
const CHATGPT_SOURCE: &str = "chatgpt_subscription";

const fn preparation_error(reason: CredentialFailureReason) -> Option<PreparationError> {
    match reason {
        CredentialFailureReason::InvalidCredential => None,
        CredentialFailureReason::InvalidStorage => {
            Some(PreparationError::CredentialStorageUnavailable)
        }
        CredentialFailureReason::TemporaryUnavailable => {
            Some(PreparationError::CredentialTemporarilyUnavailable)
        }
        CredentialFailureReason::PersistenceUncertain => {
            Some(PreparationError::CredentialRefreshPersistenceUncertain)
        }
        CredentialFailureReason::AuthorityChanged => {
            Some(PreparationError::CredentialAuthorityChanged)
        }
    }
}

pub(crate) fn preparation_failure_text(error: ChatGptError) -> String {
    let label = provider_catalog::label(&ProviderId::Codex);
    match preparation_error(classify_credential_failure(error)) {
        Some(normalized) => format!("{label}: {}", normalized.notice()),
        None => format!("{label} requires a new sign-in."),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInFailure {
    Storage(String),
    Denied,
    Expired,
    Failed,
}

pub fn sign_in_failure(error: ChatGptError) -> SignInFailure {
    match classify_credential_failure(error) {
        CredentialFailureReason::InvalidStorage | CredentialFailureReason::PersistenceUncertain => {
            SignInFailure::Storage(preparation_failure_text(error))
        }
        _ => match error {
            ChatGptError::AccessDenied | ChatGptError::ChatGptAuthorizationFailed => {
                SignInFailure::Denied
            }
            ChatGptError::LoginTimedOut => SignInFailure::Expired,
            _ => SignInFailure::Failed,
        },
    }
}

pub fn login_failure_detail(error: ChatGptError) -> String {
    match sign_in_failure(error) {
        SignInFailure::Storage(text) => text,
        SignInFailure::Denied => "authorization denied".to_owned(),
        SignInFailure::Expired => "authorization expired; run oh-fx login again".to_owned(),
        SignInFailure::Failed => "failed to sign in".to_owned(),
    }
}

pub async fn prepare_chatgpt_credential(
    oauth: &ChatGptOAuth,
    cancel: &CancellationToken,
) -> Result<Option<ChatGptAccess>, PreparationError> {
    let access = match load_chatgpt_credential(oauth, RefreshMode::IfNeeded, cancel).await {
        Ok(Some(access)) => access,
        Ok(None) => return Ok(None),
        Err(error) => {
            trace_log!(
                AUTH,
                "credential preparation failed provider=codex source={CHATGPT_SOURCE} err={error}"
            );
            return match preparation_error(classify_credential_failure(error)) {
                Some(normalized) => Err(normalized),
                None => Ok(None),
            };
        }
    };
    let blocked = access.access_token().is_empty()
        || access.refresh_after_ms() <= now_ms()
        || !valid_credential_account_id(access.account_id());
    Ok((!blocked).then_some(access))
}

pub async fn refresh_chatgpt_credential(
    oauth: &ChatGptOAuth,
    mode: RefreshMode,
    expected_account_id: &str,
    cancel: &CancellationToken,
) -> Result<Option<ChatGptAccess>, ChatGptError> {
    let loaded = load_chatgpt_credential(oauth, mode, cancel)
        .await
        .inspect_err(|error| {
            trace_log!(
                AUTH,
                "credential refresh provider failed source={CHATGPT_SOURCE} mode={} err={error}",
                mode.name()
            );
        })?;
    let Some(access) = loaded else {
        return Ok(None);
    };
    if access.account_id() != expected_account_id {
        trace_log!(
            AUTH,
            "credential refresh rejected stage=account_changed source={CHATGPT_SOURCE}"
        );
        return Err(ChatGptError::ChatGptAccountChanged);
    }
    Ok(Some(access))
}

async fn load_chatgpt_credential(
    oauth: &ChatGptOAuth,
    mode: RefreshMode,
    cancel: &CancellationToken,
) -> Result<Option<ChatGptAccess>, ChatGptError> {
    if oauth.storage_presence() == Presence::Unavailable {
        return Err(ChatGptError::CredentialStorageUnavailable);
    }
    oauth.load_access(mode, cancel).await
}

pub fn grok_login_failure_detail(error: crate::grok_oauth::GrokError) -> String {
    use crate::grok_oauth::GrokError;
    let reason = classify_grok_credential_failure(error);
    match reason {
        CredentialFailureReason::InvalidStorage | CredentialFailureReason::PersistenceUncertain => {
            let normalized =
                preparation_error(reason).expect("storage failures have preparation guidance");
            format!(
                "{}: {}",
                crate::credentials::GROK_SOURCE_LABEL,
                normalized.notice()
            )
        }
        _ => match error {
            GrokError::AccessDenied | GrokError::GrokAuthorizationFailed => {
                "authorization denied".to_owned()
            }
            GrokError::LoginTimedOut => "authorization expired; run oh-fx login again".to_owned(),
            _ => "failed to sign in".to_owned(),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredLogin {
    Missing,
    Saved { expired: bool },
    Unusable(Option<PreparationError>),
}

pub fn codex_login_saved(data_directory: PathBuf) -> bool {
    SessionStore::new(data_directory).presence() == Presence::Present
}

pub fn grok_login_saved(data_directory: PathBuf) -> bool {
    grok_session::SessionStore::new(data_directory).presence() == Presence::Present
}

pub fn stored_codex_login(data_directory: PathBuf) -> StoredLogin {
    let store = SessionStore::new(data_directory);
    let loaded = if store.presence() == Presence::Unavailable {
        Err(ChatGptError::CredentialStorageUnavailable)
    } else {
        store.load_unlocked()
    };
    match loaded {
        Ok(None) => StoredLogin::Missing,
        Ok(Some(session)) => StoredLogin::Saved {
            expired: session.expired(now_ms()),
        },
        Err(error) => StoredLogin::Unusable(preparation_error(classify_credential_failure(error))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_preparation_preserves_failure_categories_across_providers() {
        let cases = [
            (
                ChatGptError::CredentialStorageUnavailable,
                Some(PreparationError::CredentialStorageUnavailable),
            ),
            (
                ChatGptError::ConnectionFailed,
                Some(PreparationError::CredentialTemporarilyUnavailable),
            ),
            (
                ChatGptError::CredentialRefreshPersistenceUncertain,
                Some(PreparationError::CredentialRefreshPersistenceUncertain),
            ),
            (
                ChatGptError::ChatGptAccountChanged,
                Some(PreparationError::CredentialAuthorityChanged),
            ),
            (ChatGptError::CredentialRefreshRejected, None),
            (
                ChatGptError::InsecureAuthFile,
                Some(PreparationError::CredentialStorageUnavailable),
            ),
            (
                ChatGptError::LockBusy,
                Some(PreparationError::CredentialTemporarilyUnavailable),
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(
                preparation_error(classify_credential_failure(error)),
                expected,
                "{error}"
            );
        }
    }

    fn write_login(directory: &std::path::Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = directory.join("chatgpt-auth.json");
        std::fs::write(&file, contents).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn session(expires_at_ms: i64) -> String {
        format!(
            r#"{{"version":1,"access_token":"eyJhbGciOiJub25lIn0.c2F2ZWQ.c2ln","refresh_token":"rt-0123456789","expires_at_ms":{expires_at_ms},"account_id":"acct_test"}}"#
        )
    }

    #[test]
    fn a_stored_codex_login_is_read_without_refreshing_it() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("oh-fx");
        assert_eq!(stored_codex_login(data.clone()), StoredLogin::Missing);
        assert!(!codex_login_saved(data.clone()));
        write_login(&data, &session(i64::MAX));
        assert!(codex_login_saved(data.clone()));
        assert_eq!(
            stored_codex_login(data.clone()),
            StoredLogin::Saved { expired: false }
        );
        write_login(&data, &session(1));
        assert_eq!(
            stored_codex_login(data.clone()),
            StoredLogin::Saved { expired: true }
        );
        write_login(&data, "{");
        assert_eq!(
            stored_codex_login(data),
            StoredLogin::Unusable(Some(PreparationError::CredentialStorageUnavailable))
        );
    }

    #[test]
    fn grok_login_failures_preserve_storage_and_transport_categories() {
        use crate::grok_oauth::GrokError;
        for (error, expected) in [
            (
                GrokError::InvalidGrokAuthSession,
                Some(PreparationError::CredentialStorageUnavailable),
            ),
            (
                GrokError::CredentialRefreshPersistenceUncertain,
                Some(PreparationError::CredentialRefreshPersistenceUncertain),
            ),
            (
                GrokError::ConnectionFailed,
                Some(PreparationError::CredentialTemporarilyUnavailable),
            ),
            (
                GrokError::LockBusy,
                Some(PreparationError::CredentialTemporarilyUnavailable),
            ),
            (GrokError::AccessDenied, None),
        ] {
            assert_eq!(
                preparation_error(classify_grok_credential_failure(error)),
                expected,
                "{error}"
            );
        }
    }

    #[test]
    fn login_failures_name_storage_problems_and_denials_without_details() {
        assert_eq!(
            login_failure_detail(ChatGptError::CredentialStorageUnavailable),
            "Codex subscription: Saved credential storage is unavailable. Check the saved credential, then retry."
        );
        assert_eq!(
            login_failure_detail(ChatGptError::CredentialPersistenceFailed),
            "Codex subscription: Credential could not be saved. Check authentication storage before signing in again."
        );
        assert_eq!(
            login_failure_detail(ChatGptError::ChatGptAuthorizationFailed),
            "authorization denied"
        );
        assert_eq!(
            login_failure_detail(ChatGptError::LoginTimedOut),
            "authorization expired; run oh-fx login again"
        );
        assert_eq!(
            login_failure_detail(ChatGptError::ChatGptOAuthRequestFailed),
            "failed to sign in"
        );
        assert_eq!(
            preparation_failure_text(ChatGptError::CredentialRefreshRejected),
            "Codex subscription requires a new sign-in."
        );
    }
}
