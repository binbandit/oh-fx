use crate::grok_oauth::GrokError;
#[cfg(test)]
use crate::subscription_session::MUTATION_LOCK_WAIT;
pub(crate) use crate::subscription_session::Session;
use crate::subscription_session::{self, SessionError, SessionPolicy};
pub(crate) const AUTH_FILE_NAME: &str = "grok-auth.json";
#[cfg(test)]
const MUTATION_LOCK_FILE_NAME: &str = "grok-auth.lock";
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrokPolicy;
impl SessionPolicy for GrokPolicy {
    type Error = GrokError;
    const AUTH_FILE_NAME: &'static str = AUTH_FILE_NAME;
    const LOCK_FILE_NAME: &'static str = "grok-auth.lock";
    const VALIDATE_ACCOUNT_ON_WRITE: bool = true;
    fn error(error: SessionError) -> Self::Error {
        match error {
            SessionError::CredentialStorageUnavailable => GrokError::CredentialStorageUnavailable,
            SessionError::InsecureAuthFile => GrokError::InsecureAuthFile,
            SessionError::LockBusy => GrokError::LockBusy,
            SessionError::DurablePathUnsafe => GrokError::DurablePathUnsafe,
            SessionError::PrivateStatePermissionsUnsupported => {
                GrokError::PrivateStatePermissionsUnsupported
            }
            SessionError::DurableReplacePreRenameFailed => GrokError::DurableReplacePreRenameFailed,
            SessionError::DurableReplacePostRenameFailed => {
                GrokError::DurableReplacePostRenameFailed
            }
            SessionError::AccessDenied => GrokError::AccessDenied,
            SessionError::Cancelled => GrokError::Cancelled,
            SessionError::CredentialRefreshPersistenceUncertain => {
                GrokError::CredentialRefreshPersistenceUncertain
            }
            SessionError::InvalidSession => GrokError::InvalidGrokAuthSession,
        }
    }
}
pub(crate) type SessionStore = subscription_session::SessionStore<GrokPolicy>;
#[cfg(test)]
fn parse(bytes: &[u8]) -> Result<Session, GrokError> {
    subscription_session::parse(bytes).map_err(GrokPolicy::error)
}
pub(crate) fn valid_account_id(account_id: &str) -> bool {
    ofx_contract::valid_credential_account_id(account_id)
}
#[cfg(test)]
const MAX_AUTH_FILE_BYTES: usize = 64 * 1024;
#[cfg(test)]
fn stringify(session: &Session) -> Result<zeroize::Zeroizing<String>, GrokError> {
    subscription_session::encode::<GrokPolicy>(session)
}

#[cfg(test)]
use crate::secret::Secret;
#[cfg(test)]
use ofx_config::PrivateDir;
#[cfg(test)]
use std::time::{Duration, Instant};
#[cfg(test)]
mod tests;

#[cfg(test)]
use crate::subscription_access::refresh_deadline_ms;
