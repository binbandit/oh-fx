use ofx_contract::parse_strict_json_value;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::login_flow::VercelError;
use crate::oauth::FormBody;
use crate::oauth_transport::{Method, Transport};
use crate::secret::Secret;

pub(crate) const DEFAULT_SCOPE: &str = "openid offline_access";
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_INTERVAL_SECONDS: i64 = 5;
const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub(crate) issuer: String,
    pub(crate) device_authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    pub(crate) revocation_endpoint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceAuthorization {
    pub(crate) device_code: Secret,
    pub(crate) user_code: String,
    pub(crate) verification_uri: String,
    pub(crate) verification_uri_complete: Option<String>,
    pub(crate) expires_in: i64,
    pub(crate) interval: i64,
}

impl DeviceAuthorization {
    pub(crate) fn display_url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenSet {
    pub(crate) access_token: Secret,
    pub(crate) refresh_token: Option<Secret>,
    pub(crate) expires_in: i64,
    pub(crate) scope: Option<String>,
    pub(crate) token_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PollResult {
    Pending,
    SlowDown,
    Success(TokenSet),
}

pub(crate) fn missing_granted_scope<'a>(requested: &'a str, granted: &str) -> Option<&'a str> {
    let granted = granted.split(' ').filter(|scope| !scope.is_empty());
    requested
        .split(' ')
        .filter(|scope| !scope.is_empty())
        .find(|scope| !granted.clone().any(|candidate| candidate == *scope))
}

pub(crate) async fn discover(
    transport: &Transport,
    issuer_url: &str,
) -> Result<Metadata, VercelError> {
    let url = format!("{issuer_url}{DISCOVERY_PATH}");
    let response = transport.execute_get(&url).await?;
    let bytes = accepted(response.accepted, &response.body)?;
    let metadata = parse_metadata(bytes)?;
    if metadata.issuer != issuer_url {
        return Err(VercelError::InvalidIssuer);
    }
    Ok(metadata)
}

pub(crate) async fn request_device_authorization(
    transport: &Transport,
    metadata: &Metadata,
    client_id: &str,
) -> Result<DeviceAuthorization, VercelError> {
    let mut form = FormBody::default();
    form.append("client_id", client_id);
    form.append("scope", DEFAULT_SCOPE);
    let bytes = post_form(transport, &metadata.device_authorization_endpoint, &form).await?;
    parse_device_authorization(&bytes)
}

pub(crate) async fn poll_device_token(
    transport: &Transport,
    metadata: &Metadata,
    client_id: &str,
    device_code: &str,
) -> Result<PollResult, VercelError> {
    let mut form = FormBody::default();
    form.append("client_id", client_id);
    form.append("grant_type", DEVICE_CODE_GRANT);
    form.append("device_code", device_code);
    match post_form(transport, &metadata.token_endpoint, &form).await {
        Ok(bytes) => parse_token_set(&bytes).map(PollResult::Success),
        Err(VercelError::AuthorizationPending) => Ok(PollResult::Pending),
        Err(VercelError::SlowDown) => Ok(PollResult::SlowDown),
        Err(error) => Err(error),
    }
}

pub(crate) async fn refresh_token(
    transport: &Transport,
    metadata: &Metadata,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenSet, VercelError> {
    let mut form = FormBody::default();
    form.append("client_id", client_id);
    form.append("grant_type", "refresh_token");
    form.append("refresh_token", refresh_token);
    let bytes = post_form(transport, &metadata.token_endpoint, &form).await?;
    parse_token_set(&bytes)
}

pub(crate) async fn revoke_token(
    transport: &Transport,
    endpoint: &str,
    client_id: &str,
    token: &str,
    token_type_hint: &str,
) -> Result<(), VercelError> {
    let mut form = FormBody::default();
    form.append("client_id", client_id);
    form.append("token", token);
    form.append("token_type_hint", token_type_hint);
    post_form(transport, endpoint, &form).await.map(drop)
}

async fn post_form(
    transport: &Transport,
    url: &str,
    form: &FormBody,
) -> Result<Zeroizing<Vec<u8>>, VercelError> {
    let response = transport
        .execute(Method::PostForm, url, form.as_str())
        .await?;
    accepted(response.accepted, &response.body)?;
    Ok(response.body)
}

fn accepted(accepted: bool, body: &[u8]) -> Result<&[u8], VercelError> {
    if accepted {
        Ok(body)
    } else {
        Err(map_oauth_http_error(body))
    }
}

pub(crate) fn map_oauth_http_error(body: &[u8]) -> VercelError {
    let Ok(Value::Object(object)) = parse_strict_json_value(body) else {
        return VercelError::OAuthRequestFailed;
    };
    match object.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => VercelError::AuthorizationPending,
        Some("slow_down") => VercelError::SlowDown,
        Some("access_denied") => VercelError::AccessDenied,
        Some("expired_token") => VercelError::ExpiredToken,
        Some("invalid_client") => VercelError::InvalidClient,
        Some("invalid_grant") => VercelError::InvalidGrant,
        _ => VercelError::OAuthRequestFailed,
    }
}

pub(crate) fn parse_metadata(bytes: &[u8]) -> Result<Metadata, VercelError> {
    let object = parse_object(bytes)?;
    Ok(Metadata {
        issuer: required_string(&object, "issuer")?,
        device_authorization_endpoint: required_string(&object, "device_authorization_endpoint")?,
        token_endpoint: required_string(&object, "token_endpoint")?,
        revocation_endpoint: optional_string(&object, "revocation_endpoint")?,
    })
}

pub(crate) fn parse_device_authorization(bytes: &[u8]) -> Result<DeviceAuthorization, VercelError> {
    let object = parse_object(bytes)?;
    let expires_in = required_integer(&object, "expires_in")?;
    let interval = required_integer(&object, "interval").unwrap_or(DEFAULT_INTERVAL_SECONDS);
    Ok(DeviceAuthorization {
        device_code: Secret::new(required_string(&object, "device_code")?),
        user_code: required_string(&object, "user_code")?,
        verification_uri: required_string(&object, "verification_uri")?,
        verification_uri_complete: optional_string(&object, "verification_uri_complete")?,
        expires_in,
        interval,
    })
}

pub(crate) fn parse_token_set(bytes: &[u8]) -> Result<TokenSet, VercelError> {
    let object = parse_object(bytes)?;
    let token_type = required_string(&object, "token_type")?;
    if !token_type.eq_ignore_ascii_case("Bearer") {
        return Err(VercelError::InvalidOAuthResponse);
    }
    let scope = optional_string(&object, "scope")?;
    let access_token = Secret::new(required_string(&object, "access_token")?);
    let refresh_token = optional_string(&object, "refresh_token")?.map(Secret::new);
    let expires_in = required_integer(&object, "expires_in")?;
    Ok(TokenSet {
        access_token,
        refresh_token,
        expires_in,
        scope,
        token_type,
    })
}

fn parse_object(bytes: &[u8]) -> Result<Map<String, Value>, VercelError> {
    match parse_strict_json_value(bytes) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(VercelError::InvalidOAuthResponse),
        Err(_) => Err(VercelError::OAuthRequestFailed),
    }
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, VercelError> {
    match object.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(VercelError::InvalidOAuthResponse),
    }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, VercelError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(VercelError::InvalidOAuthResponse),
    }
}

fn required_integer(object: &Map<String, Value>, key: &str) -> Result<i64, VercelError> {
    object
        .get(key)
        .and_then(Value::as_i64)
        .ok_or(VercelError::InvalidOAuthResponse)
}

#[cfg(test)]
mod tests;
