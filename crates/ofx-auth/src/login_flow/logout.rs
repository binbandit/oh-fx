use super::{VercelError, VercelOAuth};
use crate::oauth::device;
use crate::oauth_session::{self, Session};
use crate::subscription_session::DeleteOutcome;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogoutResult {
    pub session_deleted: bool,
    pub local_durability_failed: bool,
    pub remote_revocation_failed: bool,
}

impl VercelOAuth {
    pub async fn logout(&self) -> Result<LogoutResult, VercelError> {
        let mut result = LogoutResult::default();
        let session = {
            let Some(mutation) = self
                .store()
                .begin_existing_mutation()
                .await
                .map_err(|_| VercelError::SessionDeleteFailed)?
            else {
                return Ok(result);
            };
            let session = match mutation.load() {
                Ok(session) => session,
                Err(VercelError::InvalidAuthSession) => None,
                Err(_) => {
                    result.remote_revocation_failed = true;
                    None
                }
            };
            match mutation.delete() {
                Ok(DeleteOutcome::Deleted) => result.session_deleted = true,
                Ok(DeleteOutcome::Missing) => {}
                Ok(DeleteOutcome::DeletedNotDurable) => {
                    result.session_deleted = true;
                    result.local_durability_failed = true;
                }
                Err(_) => result.local_durability_failed = true,
            }
            session
        };
        if let Some(session) = session
            && self.revoke(&session).await.is_err()
        {
            result.remote_revocation_failed = true;
        }
        Ok(result)
    }

    async fn revoke(&self, session: &Session) -> Result<(), VercelError> {
        let transport = self.transport()?;
        let metadata = device::discover(transport, &session.issuer).await?;
        let endpoint = metadata
            .revocation_endpoint
            .ok_or(VercelError::RevocationEndpointMissing)?;
        oauth_session::validate_e2e_endpoint(&session.issuer, &endpoint)?;
        let refresh = device::revoke_token(
            transport,
            &endpoint,
            &session.client_id,
            session.refresh_token.expose(),
            "refresh_token",
        )
        .await;
        let access = device::revoke_token(
            transport,
            &endpoint,
            &session.client_id,
            session.access_token.expose(),
            "access_token",
        )
        .await;
        refresh.and(access)
    }
}

#[cfg(test)]
mod tests;
