use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ofx_auth::{FormBody, percent_encode};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::error::McpError;
use crate::oauth_uri::OAuthUri;

const EXPIRY_SKEW_MS: i64 = 60 * 1000;
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub(crate) endpoint: String,
    pub(crate) resource: String,
    pub(crate) issuer: String,
    pub(crate) client_id: String,
    pub(crate) client_secret: Option<Zeroizing<String>>,
    pub(crate) access_token: Zeroizing<String>,
    pub(crate) refresh_token: Option<Zeroizing<String>>,
    pub(crate) scope: String,
    pub(crate) token_type: String,
    pub(crate) token_endpoint_auth_method: String,
    pub(crate) expires_at_ms: i64,
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    pub(crate) revocation_endpoint: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("endpoint", &self.endpoint)
            .field("resource", &self.resource)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .field("token_type", &self.token_type)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    pub(crate) fn needs_refresh(&self, now_ms: i64) -> bool {
        self.expires_at_ms.saturating_sub(EXPIRY_SKEW_MS) <= now_ms
    }

    pub(crate) fn bearer(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("Bearer {}", self.access_token.as_str()))
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

pub(crate) async fn refresh_credentials(
    http: &reqwest::Client,
    credentials: &Credentials,
) -> Result<Credentials, McpError> {
    let refresh_token = credentials
        .refresh_token
        .as_ref()
        .ok_or(McpError::McpRefreshTokenMissing)?;
    let mut form = FormBody::default();
    form.append("grant_type", "refresh_token");
    form.append("refresh_token", refresh_token);
    form.append("resource", &credentials.resource);
    let authorization = token_endpoint_authentication(
        &mut form,
        &credentials.token_endpoint_auth_method,
        &credentials.client_id,
        credentials.client_secret.as_deref().map(String::as_str),
    )?;
    let response = post_form(
        http,
        &credentials.token_endpoint,
        &form,
        authorization.as_ref(),
    )
    .await?;
    if response.status != StatusCode::OK {
        if response.status == StatusCode::BAD_REQUEST && refresh_rejection_is_final(&response.body)
        {
            return Err(McpError::McpRefreshRejected);
        }
        return Err(McpError::McpRefreshUnavailable);
    }
    validate_json_content_type(response.content_type.as_deref())?;
    let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(&response.body) else {
        return Err(McpError::InvalidTokenResponse);
    };
    let access_token = required_secret(&object, "access_token")?;
    let refresh_replacement = optional_secret(&object, "refresh_token")?;
    let scope_replacement = optional_string(&object, "scope")?;
    let token_type_replacement = optional_string(&object, "token_type")?;
    if token_type_replacement
        .as_deref()
        .is_some_and(|value| !value.eq_ignore_ascii_case("Bearer"))
    {
        return Err(McpError::InvalidTokenResponse);
    }
    let expires_at_ms = token_expires_at(&object, now_ms())?;
    let mut next = credentials.clone();
    next.access_token = access_token;
    if let Some(value) = refresh_replacement {
        next.refresh_token = Some(value);
    }
    if let Some(value) = scope_replacement {
        next.scope = value;
    }
    if let Some(value) = token_type_replacement {
        next.token_type = value;
    }
    next.expires_at_ms = expires_at_ms;
    Ok(next)
}

fn token_endpoint_authentication(
    form: &mut FormBody,
    method: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<Option<Zeroizing<String>>, McpError> {
    match method {
        "none" => {
            form.append("client_id", client_id);
            Ok(None)
        }
        "client_secret_post" => {
            form.append("client_id", client_id);
            form.append(
                "client_secret",
                client_secret.ok_or(McpError::ClientSecretMissing)?,
            );
            Ok(None)
        }
        "client_secret_basic" => {
            let secret = client_secret.ok_or(McpError::ClientSecretMissing)?;
            let mut pair = Zeroizing::new(String::new());
            percent_encode(&mut pair, client_id);
            pair.push(':');
            percent_encode(&mut pair, secret);
            let encoded = Zeroizing::new(STANDARD.encode(pair.as_bytes()));
            Ok(Some(Zeroizing::new(format!("Basic {}", encoded.as_str()))))
        }
        _ => Err(McpError::UnsupportedTokenEndpointAuthenticationMethod),
    }
}

fn refresh_rejection_is_final(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .is_ok_and(|value| value.get("error").and_then(Value::as_str) == Some("invalid_grant"))
}

struct OAuthResponse {
    status: StatusCode,
    content_type: Option<String>,
    body: Zeroizing<Vec<u8>>,
}

async fn post_form(
    http: &reqwest::Client,
    url: &str,
    form: &FormBody,
    authorization: Option<&Zeroizing<String>>,
) -> Result<OAuthResponse, McpError> {
    let uri = OAuthUri::parse(url).ok_or(McpError::InvalidMcpAuthEndpoint)?;
    if !uri.is_secure_or_loopback() || uri.has_userinfo || uri.fragment.is_some() {
        return Err(McpError::InsecureMcpAuthEndpoint);
    }
    let mut request = http
        .post(url)
        .timeout(REQUEST_TIMEOUT)
        .header(CONTENT_TYPE, FORM_CONTENT_TYPE)
        .body(form.as_str().to_owned());
    if let Some(authorization) = authorization {
        request = request.header(AUTHORIZATION, authorization.as_str());
    }
    let mut response = request.send().await?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > MAX_DOCUMENT_BYTES.saturating_sub(body.len()) {
            return Err(McpError::McpAuthDocumentTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(OAuthResponse {
        status,
        content_type,
        body,
    })
}

fn validate_json_content_type(content_type: Option<&str>) -> Result<(), McpError> {
    let value = content_type.ok_or(McpError::InvalidOAuthResponseContentType)?;
    let media_type = value.split(';').next().unwrap_or_default();
    if media_type
        .trim_matches([' ', '\t'])
        .eq_ignore_ascii_case("application/json")
    {
        Ok(())
    } else {
        Err(McpError::InvalidOAuthResponseContentType)
    }
}

fn required_secret(object: &Map<String, Value>, key: &str) -> Result<Zeroizing<String>, McpError> {
    match object.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(Zeroizing::new(value.clone())),
        _ => Err(McpError::InvalidOAuthResponse),
    }
}

fn optional_secret(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<Zeroizing<String>>, McpError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(Zeroizing::new(value.clone()))),
        Some(_) => Err(McpError::InvalidOAuthResponse),
    }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, McpError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(McpError::InvalidOAuthResponse),
    }
}

fn token_expires_at(object: &Map<String, Value>, now_ms: i64) -> Result<i64, McpError> {
    let Some(value) = object.get("expires_in") else {
        return Ok(i64::MAX);
    };
    let seconds = value
        .as_i64()
        .filter(|seconds| *seconds >= 0)
        .ok_or(McpError::InvalidTokenResponse)?;
    Ok(now_ms.saturating_add(seconds.saturating_mul(1000)))
}

#[cfg(test)]
mod tests;
