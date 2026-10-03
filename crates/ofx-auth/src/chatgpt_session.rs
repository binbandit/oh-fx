use crate::chatgpt_oauth::ChatGptError;
pub(crate) use crate::subscription_session::DeleteOutcome;
use crate::subscription_session::{self, SessionError, SessionPolicy};
pub(crate) use crate::subscription_session::{MUTATION_LOCK_WAIT, Session};
pub(crate) const AUTH_FILE_NAME: &str = "chatgpt-auth.json";
#[cfg(test)]
const MUTATION_LOCK_FILE_NAME: &str = "chatgpt-auth.lock";
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatGptPolicy;
impl SessionPolicy for ChatGptPolicy {
    type Error = ChatGptError;
    const AUTH_FILE_NAME: &'static str = AUTH_FILE_NAME;
    const LOCK_FILE_NAME: &'static str = "chatgpt-auth.lock";
    const VALIDATE_ACCOUNT_ON_WRITE: bool = false;
    fn error(error: SessionError) -> Self::Error {
        match error {
            SessionError::CredentialStorageUnavailable => {
                ChatGptError::CredentialStorageUnavailable
            }
            SessionError::InsecureAuthFile => ChatGptError::InsecureAuthFile,
            SessionError::LockBusy => ChatGptError::LockBusy,
            SessionError::DurablePathUnsafe => ChatGptError::DurablePathUnsafe,
            SessionError::PrivateStatePermissionsUnsupported => {
                ChatGptError::PrivateStatePermissionsUnsupported
            }
            SessionError::DurableReplacePreRenameFailed => {
                ChatGptError::DurableReplacePreRenameFailed
            }
            SessionError::DurableReplacePostRenameFailed => {
                ChatGptError::DurableReplacePostRenameFailed
            }
            SessionError::AccessDenied => ChatGptError::AccessDenied,
            SessionError::Cancelled => ChatGptError::Cancelled,
            SessionError::CredentialRefreshPersistenceUncertain => {
                ChatGptError::CredentialRefreshPersistenceUncertain
            }
            SessionError::InvalidSession => ChatGptError::InvalidChatGptAuthSession,
        }
    }
}
pub(crate) type SessionStore = subscription_session::SessionStore<ChatGptPolicy>;
pub(crate) type Mutation = subscription_session::Mutation<ChatGptPolicy>;
#[cfg(test)]
fn parse(bytes: &[u8]) -> Result<Session, ChatGptError> {
    subscription_session::parse(bytes).map_err(ChatGptPolicy::error)
}
#[cfg(test)]
fn stringify(session: &Session) -> zeroize::Zeroizing<String> {
    subscription_session::encode::<ChatGptPolicy>(session).expect("ChatGPT session encoding")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::Secret;
    use crate::subscription_access::refresh_deadline_ms;
    use ofx_config::PrivateDir;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn mutations_wait_for_the_lock_and_report_a_busy_holder() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("oh-fx");
        let store = SessionStore::new(path.clone());
        let holder = PrivateDir::open_or_create(&path).unwrap();
        let held = holder.try_lock(MUTATION_LOCK_FILE_NAME).unwrap().unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(held);
        });
        store.begin_mutation().await.unwrap();
        release.await.unwrap();

        let held = holder.try_lock(MUTATION_LOCK_FILE_NAME).unwrap().unwrap();
        let started = Instant::now();
        assert_eq!(
            store.begin_mutation().await.unwrap_err(),
            ChatGptError::LockBusy
        );
        assert!(started.elapsed() >= MUTATION_LOCK_WAIT);
        drop(held);
    }

    #[test]
    fn chatgpt_auth_session_round_trips_without_exposing_token_fields_to_structure() {
        let session = Session {
            access_token: Secret::new("header.payload.signature".to_owned()),
            refresh_token: Secret::new("refresh".to_owned()),
            expires_at_ms: 1234,
            account_id: "acct_123".to_owned(),
        };
        let encoded = stringify(&session);
        assert_eq!(
            encoded.as_str(),
            "{\"version\":1,\"access_token\":\"header.payload.signature\",\"refresh_token\":\"refresh\",\"expires_at_ms\":1234,\"account_id\":\"acct_123\"}\n"
        );
        assert_eq!(parse(encoded.as_bytes()).unwrap(), session);
        let debug = format!("{session:?}");
        assert!(!debug.contains("header.payload.signature"));
        assert!(!debug.contains("\"refresh\""));
    }

    #[test]
    fn chatgpt_session_refresh_deadline_keeps_a_one_minute_safety_margin() {
        assert_eq!(refresh_deadline_ms(100_000), 40_000);
        assert_eq!(refresh_deadline_ms(10_000), 0);
        assert_eq!(refresh_deadline_ms(i64::MIN), 0);
    }

    #[test]
    fn chatgpt_auth_session_rejects_account_identifiers_unsafe_for_http_headers() {
        assert_eq!(
            parse(br#"{"version":1,"access_token":"access","refresh_token":"refresh","expires_at_ms":1000,"account_id":"acct\r\ninjected"}"#),
            Err(ChatGptError::InvalidChatGptAuthSession)
        );
    }

    #[test]
    fn chatgpt_auth_session_rejects_other_schemas_and_shapes() {
        for invalid in [
            &br#"{"version":2,"access_token":"a","refresh_token":"r","expires_at_ms":1,"account_id":"x"}"#[..],
            br#"{"version":1.0,"access_token":"a","refresh_token":"r","expires_at_ms":1,"account_id":"x"}"#,
            br#"{"version":1,"access_token":"","refresh_token":"r","expires_at_ms":1,"account_id":"x"}"#,
            br#"{"version":1,"access_token":"a","refresh_token":"r","expires_at_ms":"1","account_id":"x"}"#,
            br#"{"version":1,"version":1,"access_token":"a","refresh_token":"r","expires_at_ms":1,"account_id":"x"}"#,
            b"[]",
        ] {
            assert_eq!(parse(invalid), Err(ChatGptError::InvalidChatGptAuthSession));
        }
    }
}
