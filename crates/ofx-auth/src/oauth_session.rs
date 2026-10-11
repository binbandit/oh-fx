use ofx_contract::parse_strict_json_value;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::login_flow::VercelError;
use crate::oauth::is_loopback_http_url;
use crate::secret::Secret;
use crate::subscription_session::{self, SessionError, SessionPolicy};

pub(crate) const ISSUER: &str = "https://vercel.com";
pub(crate) const DEFAULT_CLIENT_ID: &str = "cl_zzh5hiOZbwJ9bfqEcYqPIJv3TaPaEYL0";
pub(crate) const CLIENT_ID_VARIABLE: &str = "OH_FX_OAUTH_CLIENT_ID";
pub(crate) const E2E_ISSUER_VARIABLE: &str = "OH_FX_E2E_OAUTH_ISSUER_URL";
const SCHEMA_VERSION: i64 = 1;
const EXPIRY_SKEW_MS: i64 = 60 * 1000;
const BLANK: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    pub(crate) issuer: String,
    pub(crate) client_id: String,
    pub(crate) access_token: Secret,
    pub(crate) refresh_token: Secret,
    pub(crate) expires_at_ms: i64,
    pub(crate) scope: String,
    pub(crate) token_type: String,
    pub(crate) team_slug: Option<String>,
    pub(crate) team_id: Option<String>,
}

impl Session {
    pub(crate) fn expired(&self, now_ms: i64) -> bool {
        refresh_deadline_ms(self.expires_at_ms) <= now_ms
    }
}

pub(crate) fn refresh_deadline_ms(expires_at_ms: i64) -> i64 {
    expires_at_ms.saturating_sub(EXPIRY_SKEW_MS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VercelPolicy;

impl SessionPolicy for VercelPolicy {
    type Error = VercelError;
    type Session = Session;
    const TRACE_PREFIX: &'static str = "";
    const AUTH_FILE_NAME: &'static str = "auth.json";
    const LOCK_FILE_NAME: &'static str = "auth.lock";

    fn error(error: SessionError) -> VercelError {
        match error {
            SessionError::CredentialStorageUnavailable | SessionError::AccessDenied => {
                VercelError::CredentialStorageUnavailable
            }
            SessionError::InsecureAuthFile => VercelError::InsecureAuthFile,
            SessionError::InvalidSession => VercelError::InvalidAuthSession,
            SessionError::LockBusy => VercelError::LockBusy,
            SessionError::DurablePathUnsafe => VercelError::DurablePathUnsafe,
            SessionError::PrivateStatePermissionsUnsupported => {
                VercelError::PrivateStatePermissionsUnsupported
            }
            SessionError::DurableReplacePreRenameFailed => {
                VercelError::DurableReplacePreRenameFailed
            }
            SessionError::DurableReplacePostRenameFailed => {
                VercelError::DurableReplacePostRenameFailed
            }
            SessionError::Cancelled => VercelError::Cancelled,
            SessionError::CredentialRefreshPersistenceUncertain => {
                VercelError::OAuthSessionCleanupUncertain
            }
        }
    }

    fn parse(bytes: &[u8]) -> Result<Session, SessionError> {
        parse(bytes)
    }

    fn encode(session: &Session) -> Result<Zeroizing<String>, VercelError> {
        Ok(stringify(session))
    }
}

pub(crate) type SessionStore = subscription_session::SessionStore<VercelPolicy>;
pub(crate) type Mutation = subscription_session::Mutation<VercelPolicy>;

pub(crate) fn configured_client_id(lookup: &dyn Fn(&str) -> Option<String>) -> String {
    lookup(CLIENT_ID_VARIABLE)
        .filter(|value| !value.trim_matches(BLANK).is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned())
}

pub(crate) fn configured_issuer_url(lookup: &dyn Fn(&str) -> Option<String>) -> String {
    lookup(E2E_ISSUER_VARIABLE)
        .map(|value| value.trim_end_matches('/').to_owned())
        .filter(|value| is_loopback_origin(value))
        .unwrap_or_else(|| ISSUER.to_owned())
}

pub(crate) fn is_loopback_e2e_issuer(url: &str) -> bool {
    url != ISSUER && is_loopback_origin(url)
}

pub(crate) fn validate_e2e_endpoint(issuer_url: &str, endpoint: &str) -> Result<(), VercelError> {
    if is_loopback_e2e_issuer(issuer_url) && !is_loopback_http_url(endpoint) {
        return Err(VercelError::InvalidE2EOAuthEndpoint);
    }
    Ok(())
}

fn is_loopback_origin(url: &str) -> bool {
    is_loopback_http_url(url)
        && url
            .split_once("://")
            .is_some_and(|(_, rest)| !rest.contains(['/', '?', '#']))
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Session, SessionError> {
    let Ok(Value::Object(object)) = parse_strict_json_value(bytes) else {
        return Err(SessionError::InvalidSession);
    };
    let version = object.get("version").filter(|value| value.is_i64());
    if version.and_then(Value::as_i64) != Some(SCHEMA_VERSION) {
        return Err(SessionError::InvalidSession);
    }
    let issuer = required_string(&object, "issuer")?;
    if issuer != ISSUER && !is_loopback_e2e_issuer(&issuer) {
        return Err(SessionError::InvalidSession);
    }
    let expires_at_ms = object
        .get("expires_at_ms")
        .filter(|value| value.is_i64())
        .and_then(Value::as_i64)
        .ok_or(SessionError::InvalidSession)?;
    Ok(Session {
        issuer,
        client_id: required_string(&object, "client_id")?,
        access_token: Secret::new(required_string(&object, "access_token")?),
        refresh_token: Secret::new(required_string(&object, "refresh_token")?),
        expires_at_ms,
        scope: required_string(&object, "scope")?,
        token_type: required_string(&object, "token_type")?,
        team_slug: optional_string(&object, "team_slug")?,
        team_id: optional_string(&object, "team_id")?,
    })
}

pub(crate) fn stringify(session: &Session) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::from("{\"version\":1"));
    push_field(&mut out, "issuer", &session.issuer);
    push_field(&mut out, "client_id", &session.client_id);
    push_field(&mut out, "access_token", session.access_token.expose());
    push_field(&mut out, "refresh_token", session.refresh_token.expose());
    out.push_str(",\"expires_at_ms\":");
    out.push_str(&session.expires_at_ms.to_string());
    push_field(&mut out, "scope", &session.scope);
    push_field(&mut out, "token_type", &session.token_type);
    if let Some(team_slug) = &session.team_slug {
        push_field(&mut out, "team_slug", team_slug);
    }
    if let Some(team_id) = &session.team_id {
        push_field(&mut out, "team_id", team_id);
    }
    out.push_str("}\n");
    out
}

fn push_field(out: &mut String, name: &str, value: &str) {
    out.push_str(",\"");
    out.push_str(name);
    out.push_str("\":");
    out.push_str(&Zeroizing::new(
        serde_json::to_string(value).unwrap_or_default(),
    ));
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, SessionError> {
    match object.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(SessionError::InvalidSession),
    }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, SessionError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(SessionError::InvalidSession),
    }
}

#[cfg(test)]
mod tests;
