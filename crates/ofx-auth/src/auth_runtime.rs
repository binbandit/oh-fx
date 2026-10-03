use ofx_config::ProviderId;
use ofx_contract::valid_credential_account_id;
use tokio_util::sync::CancellationToken;

use crate::chatgpt_oauth::{ChatGptAccess, ChatGptError, ChatGptOAuth, RefreshMode};
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
        GrokError::AccessDenied | GrokError::CredentialRefreshRejected => {
            CredentialFailureReason::InvalidCredential
        }
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
        GrokError::GrokAccountChanged => CredentialFailureReason::AuthorityChanged,
        _ => CredentialFailureReason::TemporaryUnavailable,
    }
}

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

pub fn login_failure_detail(error: ChatGptError) -> String {
    match classify_credential_failure(error) {
        CredentialFailureReason::InvalidStorage | CredentialFailureReason::PersistenceUncertain => {
            preparation_failure_text(error)
        }
        _ => match error {
            ChatGptError::AccessDenied | ChatGptError::ChatGptAuthorizationFailed => {
                "authorization denied".to_owned()
            }
            ChatGptError::LoginTimedOut => {
                "authorization expired; run oh-fx login again".to_owned()
            }
            _ => "failed to sign in".to_owned(),
        },
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
    let Some(access) = load_chatgpt_credential(oauth, mode, cancel).await? else {
        return Ok(None);
    };
    if access.account_id() != expected_account_id {
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

pub async fn prepare_grok_credential(
    oauth: &crate::grok_oauth::GrokOAuth,
    cancel: &CancellationToken,
) -> Result<Option<crate::grok_oauth::GrokAccess>, PreparationError> {
    use crate::grok_oauth::{GrokError, GrokRefreshMode};
    let result = if oauth.storage_presence() == Presence::Unavailable {
        Err(GrokError::CredentialStorageUnavailable)
    } else {
        match oauth.load_access(GrokRefreshMode::Stored, cancel).await {
            Ok(Some(stored)) => {
                refresh_grok_credential(
                    oauth,
                    GrokRefreshMode::IfNeeded,
                    stored.account_id(),
                    cancel,
                )
                .await
            }
            Ok(None) => Ok(None),
            Err(error) => Err(error),
        }
    };
    let access = match result {
        Ok(Some(access)) => access,
        Ok(None) => return Ok(None),
        Err(error) => {
            return match preparation_error(classify_grok_credential_failure(error)) {
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

pub(crate) async fn refresh_grok_credential(
    oauth: &crate::grok_oauth::GrokOAuth,
    mode: crate::grok_oauth::GrokRefreshMode,
    expected_account_id: &str,
    cancel: &CancellationToken,
) -> Result<Option<crate::grok_oauth::GrokAccess>, crate::grok_oauth::GrokError> {
    let Some(access) = oauth.load_access(mode, cancel).await? else {
        return Ok(None);
    };
    if access.account_id() != expected_account_id {
        return Err(crate::grok_oauth::GrokError::GrokAccountChanged);
    }
    Ok(Some(access))
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

    #[test]
    fn grok_credential_failures_preserve_storage_authority_and_refresh_categories() {
        use crate::grok_oauth::GrokError;
        for (error, expected) in [
            (
                GrokError::InvalidGrokAuthSession,
                Some(PreparationError::CredentialStorageUnavailable),
            ),
            (
                GrokError::GrokAccountChanged,
                Some(PreparationError::CredentialAuthorityChanged),
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
            (GrokError::CredentialRefreshRejected, None),
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
