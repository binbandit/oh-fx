use std::time::Duration;

use ofx_trace::trace_log;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::{GatewayKey, GatewaySource};
use crate::chatgpt_oauth::{REFRESH_SAVE_TIME, RefreshMode};
use crate::login_flow::{VercelError, VercelOAuth};
use crate::oauth;
use crate::oauth::device;
use crate::oauth_session::{self, Mutation, Session};
use crate::oauth_transport::REQUEST_TIMEOUT;
use crate::session_presence::Presence;
use crate::subscription_access::now_ms;

const AUTH: &str = "auth";
pub const VERCEL_SENT_REFRESH_LIMIT: Duration = REQUEST_TIMEOUT.saturating_add(REFRESH_SAVE_TIME);

impl VercelOAuth {
    pub(crate) fn load_stored_credential(&self) -> Result<Option<GatewayKey>, VercelError> {
        self.require_source_storage()?;
        Ok(self
            .store()
            .load_unlocked()?
            .map(|session| credential(session, None)))
    }

    pub async fn load_credential(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<GatewayKey>, VercelError> {
        self.require_source_storage()?;
        let Some(session) = self.store().load_unlocked()? else {
            return Ok(None);
        };
        if !session.expired(now_ms()) {
            return Ok(Some(credential(session, None)));
        }
        self.load_locked(RefreshMode::IfNeeded, cancel).await
    }

    pub async fn refresh_credential(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<GatewayKey>, VercelError> {
        self.require_source_storage()?;
        self.load_locked(RefreshMode::Force, cancel).await
    }

    async fn load_locked(
        &self,
        mode: RefreshMode,
        cancel: &CancellationToken,
    ) -> Result<Option<GatewayKey>, VercelError> {
        let begun = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(VercelError::Cancelled),
            begun = self.store().begin_existing_mutation() => begun,
        };
        let Some(mutation) = begun? else {
            return Ok(None);
        };
        let Some(mut session) = mutation.load()? else {
            return Ok(None);
        };
        let mut refreshed_at_ms = None;
        if mode == RefreshMode::Force || session.expired(now_ms()) {
            self.refresh_session(&mutation, &mut session, cancel)
                .await?;
            refreshed_at_ms = Some(now_ms());
        }
        Ok(Some(credential(session, refreshed_at_ms)))
    }

    fn require_source_storage(&self) -> Result<(), VercelError> {
        if self.store().presence() == Presence::Unavailable {
            trace_log!(AUTH, "credential storage unavailable source=fx_login");
            return Err(VercelError::CredentialStorageUnavailable);
        }
        Ok(())
    }

    pub(crate) async fn refresh_session(
        &self,
        mutation: &Mutation,
        session: &mut Session,
        cancel: &CancellationToken,
    ) -> Result<(), VercelError> {
        mutation.require_writable()?;
        let transport = self.transport()?;
        let metadata = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(VercelError::Cancelled),
            metadata = device::discover(transport, &session.issuer) => metadata?,
        };
        oauth_session::validate_e2e_endpoint(&session.issuer, &metadata.token_endpoint)?;
        if cancel.is_cancelled() {
            return Err(VercelError::Cancelled);
        }
        let refreshed = match device::refresh_token(
            transport,
            &metadata,
            &session.client_id,
            session.refresh_token.expose(),
        )
        .await
        {
            Ok(refreshed) => refreshed,
            Err(VercelError::InvalidOAuthResponse) => {
                return retire(mutation, "InvalidOAuthResponse");
            }
            Err(error) => {
                if requires_sign_in(error) {
                    trace_log!(AUTH, "retiring terminal fx login session reason={error}");
                    mutation.retire()?;
                }
                return Err(error);
            }
        };
        let Some(rotated) = refreshed.refresh_token else {
            return retire(mutation, "NoRefreshToken");
        };
        let Ok(expires_at_ms) = oauth::expiry_timestamp_ms(now_ms(), refreshed.expires_in) else {
            return retire(mutation, "InvalidOAuthResponse");
        };
        let replacement = Session {
            access_token: refreshed.access_token,
            refresh_token: rotated,
            expires_at_ms,
            scope: refreshed.scope.unwrap_or_else(|| session.scope.clone()),
            token_type: refreshed.token_type,
            ..session.clone()
        };
        if let Err(error) = mutation.save(&replacement) {
            trace_log!(
                AUTH,
                "retiring fx login session after refresh save failed err={error}"
            );
            if let Err(cleanup) = mutation.retire() {
                trace_log!(AUTH, "fx login session retirement failed err={cleanup}");
            }
            return Err(VercelError::OAuthSessionCleanupUncertain);
        }
        *session = replacement;
        Ok(())
    }
}

fn retire(mutation: &Mutation, reason: &str) -> Result<(), VercelError> {
    trace_log!(AUTH, "retiring fx login session reason={reason}");
    mutation.retire()?;
    Err(VercelError::NoRefreshToken)
}

const fn requires_sign_in(error: VercelError) -> bool {
    matches!(
        error,
        VercelError::InvalidGrant
            | VercelError::AccessDenied
            | VercelError::ExpiredToken
            | VercelError::InvalidClient
    )
}

fn credential(session: Session, refreshed_at_ms: Option<i64>) -> GatewayKey {
    GatewayKey {
        source: GatewaySource::FxLogin,
        refresh_after_ms: Some(refresh_after_ms(session.expires_at_ms, refreshed_at_ms)),
        secret: Zeroizing::new(session.access_token.into_inner()),
        team_id: session.team_id,
        team_slug: session.team_slug,
    }
}

fn refresh_after_ms(expires_at_ms: i64, refreshed_at_ms: Option<i64>) -> i64 {
    let deadline = oauth_session::refresh_deadline_ms(expires_at_ms);
    match refreshed_at_ms {
        Some(refreshed_at) if deadline <= refreshed_at => expires_at_ms,
        _ => deadline,
    }
}

#[cfg(test)]
mod tests;
