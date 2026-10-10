use crate::browser_callback::ParseResult;
use crate::grok_session::{Session, SessionStore, valid_account_id};
use crate::oauth::OAuthError;
use crate::oauth::{self, FormBody, parse_object, query_value_non_empty};
use crate::oauth_transport::TransportError;
use crate::oauth_transport::{Method, Transport};
use crate::secret::Secret;
use crate::subscription_access::now_ms;

use crate::subscription_session::DeleteOutcome;
use ofx_trace::trace_log;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use subtle::ConstantTimeEq;
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GrokError {
    #[error("CredentialStorageUnavailable")]
    CredentialStorageUnavailable,
    #[error("LockBusy")]
    LockBusy,
    #[error("InsecureAuthFile")]
    InsecureAuthFile,
    #[error("InvalidGrokAuthSession")]
    InvalidGrokAuthSession,
    #[error("DurablePathUnsafe")]
    DurablePathUnsafe,
    #[error("PrivateStatePermissionsUnsupported")]
    PrivateStatePermissionsUnsupported,
    #[error("DurableReplacePreRenameFailed")]
    DurableReplacePreRenameFailed,
    #[error("DurableReplacePostRenameFailed")]
    DurableReplacePostRenameFailed,
    #[error("AccessDenied")]
    AccessDenied,
    #[error("CredentialRefreshPersistenceUncertain")]
    CredentialRefreshPersistenceUncertain,
    #[error("CredentialPersistenceFailed")]
    CredentialPersistenceFailed,
    #[error("GrokOAuthRequestFailed")]
    GrokOAuthRequestFailed,
    #[error("InvalidGrokOAuthResponse")]
    InvalidGrokOAuthResponse,
    #[error("GrokOAuthCallbackPortUnavailable")]
    GrokOAuthCallbackPortUnavailable,
    #[error("GrokOAuthCallbackListenerFailed")]
    GrokOAuthCallbackListenerFailed,
    #[error("GrokAuthorizationFailed")]
    GrokAuthorizationFailed,
    #[error("GrokLoginBusy")]
    GrokLoginBusy,
    #[error("LoginTimedOut")]
    LoginTimedOut,
    #[error("Cancelled")]
    Cancelled,
    #[error("OAuthTransportUnavailable")]
    OAuthTransportUnavailable,
    #[error("ConnectionFailed")]
    ConnectionFailed,
    #[error("Timeout")]
    Timeout,
    #[error("OAuthResponseTooLarge")]
    OAuthResponseTooLarge,
    #[error("GrokOAuthStateMismatch")]
    GrokOAuthStateMismatch,
    #[error("InvalidGrokOAuthCallback")]
    InvalidGrokOAuthCallback,
    #[error("InvalidGrokUserInfoResponse")]
    InvalidGrokUserInfoResponse,
    #[error("GrokUserInfoRequestFailed")]
    GrokUserInfoRequestFailed,
    #[error("InvalidGrokAuthorizationCode")]
    InvalidGrokAuthorizationCode,
    #[error("InvalidGrokOAuthEndpoint")]
    InvalidGrokOAuthEndpoint,
    #[error("GrokLoginStateUnavailable")]
    GrokLoginStateUnavailable,
    #[error("GrokAuthorizationCodeTooLong")]
    GrokAuthorizationCodeTooLong,
    #[error("ReadFailed")]
    ReadFailed,
    #[error("WriteFailed")]
    WriteFailed,
    #[error("RandomSourceUnavailable")]
    RandomSourceUnavailable,
}

impl From<TransportError> for GrokError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::Unavailable => Self::OAuthTransportUnavailable,
            TransportError::ConnectionFailed => Self::ConnectionFailed,
            TransportError::Timeout => Self::Timeout,
            TransportError::ResponseTooLarge => Self::OAuthResponseTooLarge,
        }
    }
}

impl From<OAuthError> for GrokError {
    fn from(error: OAuthError) -> Self {
        match error {
            OAuthError::InvalidOAuthResponse => Self::InvalidGrokOAuthResponse,
            OAuthError::RandomSourceUnavailable => Self::RandomSourceUnavailable,
        }
    }
}

mod run_login;
mod sign_in;
use sign_in::GrokSignIn;

#[cfg(test)]
mod tests;

const AUTH: &str = "auth";
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const BROWSER_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_MANUAL_CODE_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokEndpoints {
    pub issuer: String,
    pub token_url: String,
    pub userinfo_url: String,
    pub revoke_url: String,
}
impl Default for GrokEndpoints {
    fn default() -> Self {
        Self {
            issuer: "https://auth.x.ai".to_owned(),
            token_url: "https://auth.x.ai/oauth2/token".to_owned(),
            userinfo_url: "https://auth.x.ai/oauth2/userinfo".to_owned(),
            revoke_url: "https://auth.x.ai/oauth2/revoke".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GrokOAuth {
    store: SessionStore,
    transport: Transport,
    endpoints: GrokEndpoints,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrokLogoutResult {
    pub deletion: DeleteOutcome,
    pub revocation_failed: bool,
}

impl GrokOAuth {
    pub fn new(
        directory: PathBuf,
        user_agent: &str,
        endpoints: GrokEndpoints,
    ) -> Result<Self, GrokError> {
        endpoints.validate()?;
        Ok(Self {
            store: SessionStore::new(directory),
            transport: Transport::new(user_agent)?,
            endpoints,
        })
    }

    pub(crate) async fn start_sign_in(&self) -> Result<GrokSignIn, GrokError> {
        GrokSignIn::new(self.clone()).await
    }

    pub fn has_saved_login(&self) -> bool {
        self.store.presence() == crate::session_presence::Presence::Present
    }

    pub async fn logout(&self) -> Result<GrokLogoutResult, GrokError> {
        let Some(mutation) = self.store.begin_existing_mutation().await? else {
            return Ok(GrokLogoutResult {
                deletion: DeleteOutcome::Missing,
                revocation_failed: false,
            });
        };
        let revocation_failed = match mutation.load() {
            Ok(Some(session)) => self
                .revoke_token(session.refresh_token.expose())
                .await
                .is_err(),
            Ok(None) => false,
            Err(error) => {
                trace_log!(
                    AUTH,
                    "Grok logout could not load the saved credential err={error}"
                );
                true
            }
        };
        Ok(GrokLogoutResult {
            deletion: mutation.delete()?,
            revocation_failed,
        })
    }

    async fn fetch_account_id(&self, token: &str) -> Result<String, GrokError> {
        let response = self
            .transport
            .execute_authorized_get(&self.endpoints.userinfo_url, token)
            .await?;
        if !response.accepted {
            return Err(GrokError::GrokUserInfoRequestFailed);
        }
        let object =
            parse_object(&response.body).map_err(|_| GrokError::InvalidGrokUserInfoResponse)?;
        match object.get("sub") {
            Some(Value::String(account)) if valid_account_id(account) => Ok(account.clone()),
            _ => Err(GrokError::InvalidGrokUserInfoResponse),
        }
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
        redirect: &str,
    ) -> Result<Session, GrokError> {
        let mut form = FormBody::default();
        for (key, value) in [
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect),
        ] {
            form.append(key, value);
        }
        let response = self
            .transport
            .execute(Method::PostForm, &self.endpoints.token_url, form.as_str())
            .await?;
        if !response.accepted {
            trace_log!(
                AUTH,
                "Grok OAuth request rejected url={}",
                self.endpoints.token_url
            );
            return Err(GrokError::GrokOAuthRequestFailed);
        }
        let token = oauth::parse_browser_token_set(&response.body)?;
        let expiry = token
            .expires_in
            .ok_or(GrokError::InvalidGrokOAuthResponse)?;
        let account_id = self.fetch_account_id(token.access_token.expose()).await?;
        Ok(Session {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at_ms: oauth::expiry_timestamp_ms(now_ms(), expiry)?,
            account_id,
        })
    }

    async fn revoke_token(&self, token: &str) -> Result<(), GrokError> {
        let mut form = FormBody::default();
        form.append("token", token);
        form.append("client_id", CLIENT_ID);
        let response = self
            .transport
            .execute(Method::PostForm, &self.endpoints.revoke_url, form.as_str())
            .await?;
        if response.accepted {
            Ok(())
        } else {
            trace_log!(
                AUTH,
                "Grok OAuth request rejected url={}",
                self.endpoints.revoke_url
            );
            Err(GrokError::GrokOAuthRequestFailed)
        }
    }
}

fn build_browser_authorization_url(
    issuer: &str,
    redirect: &str,
    challenge: &str,
    state: &str,
) -> String {
    let mut form = FormBody::default();
    for (key, value) in [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect),
        ("scope", BROWSER_SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("referrer", "fx"),
    ] {
        form.append(key, value);
    }
    format!(
        "{}/oauth2/authorize?{}",
        issuer.trim_end_matches('/'),
        form.as_str()
    )
}
fn classify_browser_callback(target: &str, state: &str) -> ParseResult<Secret, GrokError> {
    match parse_browser_callback_target(target, state) {
        Ok(code) => ParseResult::Accepted(code),
        Err(GrokError::InvalidGrokOAuthCallback) => ParseResult::Unrelated,
        Err(error) => ParseResult::Failed(error),
    }
}
fn parse_browser_callback_target(target: &str, expected: &str) -> Result<Secret, GrokError> {
    let query = target
        .strip_prefix("/callback?")
        .filter(|_| !target.contains('#'))
        .ok_or(GrokError::InvalidGrokOAuthCallback)?;
    let denied = query_value_non_empty(query, "error").is_ok();
    let code = if denied {
        None
    } else {
        Some(
            query_value_non_empty(query, "code")
                .map_err(|_| GrokError::InvalidGrokOAuthCallback)?,
        )
    };
    let state =
        query_value_non_empty(query, "state").map_err(|_| GrokError::InvalidGrokOAuthCallback)?;
    if !bool::from(state.as_bytes().ct_eq(expected.as_bytes())) {
        return Err(GrokError::GrokOAuthStateMismatch);
    }
    code.map(|code| Secret::new(code.to_string()))
        .ok_or(GrokError::GrokAuthorizationFailed)
}

impl GrokEndpoints {
    fn validate(&self) -> Result<(), GrokError> {
        let defaults = Self::default();
        for (url, default) in [
            (&self.issuer, &defaults.issuer),
            (&self.token_url, &defaults.token_url),
            (&self.userinfo_url, &defaults.userinfo_url),
            (&self.revoke_url, &defaults.revoke_url),
        ] {
            if url != default && !oauth::is_loopback_http_url(url) {
                return Err(GrokError::InvalidGrokOAuthEndpoint);
            }
        }
        Ok(())
    }
}
