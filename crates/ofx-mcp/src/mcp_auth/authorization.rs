use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ofx_auth::{
    AwaitError, CallbackListener, CallbackResponse, Classifier, FormBody, ParseResult, QueryError,
    pkce_challenge, query_value,
};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::metadata::{
    AuthorizationMetadata, MetadataOutcome, contains, discover_authorization_metadata,
    discover_resource_metadata, validate_authorization_metadata_urls,
    validate_oauth_url_for_resource,
};
use super::{
    Credentials, Payload, now_ms, optional_secret, optional_string, parse_json, request,
    required_secret, token_endpoint_authentication, token_expires_at, validate_json_content_type,
};
use crate::error::McpError;
use crate::oauth_uri::canonical_resource;

const CALLBACK_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_SCOPE_TOKENS: usize = 64;
const MAX_SCOPE_TOKEN_BYTES: usize = 256;
const VERIFIER_ENTROPY_BYTES: usize = 48;
const STATE_ENTROPY_BYTES: usize = 32;
const CLIENT_NAME: &str = "oh-fx";

#[derive(Clone, Copy, Default)]
pub(crate) struct ClientConfig<'a> {
    pub(crate) resource: Option<&'a str>,
    pub(crate) issuer: Option<&'a str>,
    pub(crate) client_id: Option<&'a str>,
    pub(crate) client_secret: Option<&'a str>,
    pub(crate) client_metadata_url: Option<&'a str>,
    pub(crate) scopes: &'a [String],
    pub(crate) callback_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthorizationResult {
    Credentials(Box<Credentials>),
    IssuerMismatch,
}

struct ClientRegistration {
    client_id: String,
    client_secret: Option<Zeroizing<String>>,
    token_endpoint_auth_method: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AuthorizationResponse {
    code: Zeroizing<String>,
    state: Zeroizing<String>,
    issuer: Option<String>,
}

impl fmt::Debug for AuthorizationResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizationResponse")
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

pub(crate) async fn authorize_interactive(
    http: &reqwest::Client,
    endpoint: &str,
    config: &ClientConfig<'_>,
    previous_scope: Option<&str>,
    open_url: &(dyn Fn(&str) -> bool + Sync),
    cancel: &CancellationToken,
) -> Result<AuthorizationResult, McpError> {
    let listener = CallbackListener::bind(&[config.callback_port.unwrap_or(0)])
        .await
        .map_err(|_| McpError::McpCallbackPortUnavailable)?;
    let redirect_uri = match config.callback_port {
        Some(port) => format!("http://localhost:{port}/callback"),
        None => format!("http://127.0.0.1:{}/callback", listener.port()),
    };
    let endpoint = canonical_resource(endpoint)?;
    let mut resource = match config.resource {
        Some(configured) => canonical_resource(configured)?,
        None => endpoint.clone(),
    };
    let protected = discover_resource_metadata(http, &resource).await?;
    if resource != protected.resource {
        resource.clone_from(&protected.resource);
    }
    let issuer = config
        .issuer
        .unwrap_or(protected.authorization_servers[0].as_str());
    validate_oauth_url_for_resource(issuer, &resource)?;
    let metadata = match discover_authorization_metadata(http, issuer).await? {
        MetadataOutcome::Metadata(metadata) => *metadata,
        MetadataOutcome::IssuerMismatch => return Ok(AuthorizationResult::IssuerMismatch),
    };
    validate_authorization_metadata_urls(&metadata, &resource)?;
    if !metadata.supports_s256() {
        return Err(McpError::PkceS256NotSupported);
    }
    let registration =
        resolve_client_registration(http, &metadata, &resource, config, &redirect_uri).await?;
    let scope = requested_scope(
        config.scopes,
        &protected.scopes_supported,
        previous_scope,
        contains(&metadata.scopes_supported, "offline_access"),
    )?;
    let verifier = random_url_safe::<VERIFIER_ENTROPY_BYTES>()?;
    let state = random_url_safe::<STATE_ENTROPY_BYTES>()?;
    let url = authorization_url(
        &metadata.authorization_endpoint,
        &[
            ("response_type", "code"),
            ("client_id", &registration.client_id),
            ("redirect_uri", &redirect_uri),
            ("resource", &resource),
            ("state", &state),
            ("code_challenge", &pkce_challenge(&verifier)),
            ("code_challenge_method", "S256"),
        ],
        scope.as_deref(),
    );
    if !open_url(&url) {
        return Err(McpError::McpAuthorizationBrowserOpenFailed);
    }
    let callback = wait_for_callback(&listener, cancel).await?;
    if validate_authorization_response(
        &state,
        &metadata.issuer,
        metadata.authorization_response_iss_parameter_supported,
        &callback,
    )? == IssuerCheck::Mismatch
    {
        return Ok(AuthorizationResult::IssuerMismatch);
    }
    exchange_authorization_code(
        http,
        metadata,
        registration,
        CodeExchange {
            endpoint,
            resource,
            code: &callback.code,
            verifier: &verifier,
            redirect_uri: &redirect_uri,
            scope: scope.as_deref(),
        },
    )
    .await
    .map(|credentials| AuthorizationResult::Credentials(Box::new(credentials)))
}

fn random_url_safe<const BYTES: usize>() -> Result<Zeroizing<String>, McpError> {
    let mut entropy = Zeroizing::new([0_u8; BYTES]);
    getrandom::fill(entropy.as_mut_slice()).map_err(|_| McpError::RandomSourceUnavailable)?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(entropy.as_slice())))
}

async fn resolve_client_registration(
    http: &reqwest::Client,
    metadata: &AuthorizationMetadata,
    resource: &str,
    config: &ClientConfig<'_>,
    redirect_uri: &str,
) -> Result<ClientRegistration, McpError> {
    if let Some(client_id) = config.client_id {
        return Ok(ClientRegistration {
            client_id: client_id.to_owned(),
            client_secret: config
                .client_secret
                .map(|secret| Zeroizing::new(secret.to_owned())),
            token_endpoint_auth_method: token_endpoint_auth_method(
                metadata,
                config.client_secret.is_some(),
            )?
            .to_owned(),
        });
    }
    if metadata.client_id_metadata_document_supported
        && let Some(url) = config.client_metadata_url
    {
        validate_oauth_url_for_resource(url, resource)?;
        return Ok(ClientRegistration {
            client_id: url.to_owned(),
            client_secret: None,
            token_endpoint_auth_method: token_endpoint_auth_method(metadata, false)?.to_owned(),
        });
    }
    let registration_endpoint = metadata
        .registration_endpoint
        .as_deref()
        .ok_or(McpError::ClientRegistrationUnavailable)?;
    let method = token_endpoint_auth_method(metadata, false)?;
    let mut grant_types = String::from("\"authorization_code\"");
    if metadata.supports_refresh_token() {
        grant_types.push_str(",\"refresh_token\"");
    }
    let payload = format!(
        "{{\"client_name\":{},\"application_type\":\"native\",\"redirect_uris\":[{}],\"response_types\":[\"code\"],\"grant_types\":[{grant_types}],\"token_endpoint_auth_method\":{}}}",
        Value::from(CLIENT_NAME),
        Value::from(redirect_uri),
        Value::from(method),
    );
    let response = request(
        http,
        Method::POST,
        registration_endpoint,
        Payload::Json(&payload),
        None,
    )
    .await?;
    if response.status != StatusCode::CREATED && response.status != StatusCode::OK {
        return Err(McpError::ClientRegistrationFailed);
    }
    validate_json_content_type(response.content_type.as_deref())?;
    let Value::Object(object) = parse_json(&response.body)? else {
        return Err(McpError::ClientRegistrationFailed);
    };
    let client_id = required_secret(&object, "client_id")?;
    let client_secret = optional_secret(&object, "client_secret")?;
    let returned_method = match object.get("token_endpoint_auth_method") {
        Some(Value::String(value)) if !value.is_empty() => value.as_str(),
        _ => method,
    };
    Ok(ClientRegistration {
        client_id: client_id.to_string(),
        client_secret,
        token_endpoint_auth_method: returned_method.to_owned(),
    })
}

fn token_endpoint_auth_method(
    metadata: &AuthorizationMetadata,
    has_secret: bool,
) -> Result<&'static str, McpError> {
    let basic = metadata.supports_method("client_secret_basic");
    let post = metadata.supports_method("client_secret_post");
    if has_secret && basic {
        Ok("client_secret_basic")
    } else if has_secret && post {
        Ok("client_secret_post")
    } else if metadata.supports_method("none") || (!has_secret && metadata.supports_s256()) {
        Ok("none")
    } else if basic {
        Ok("client_secret_basic")
    } else if post {
        Ok("client_secret_post")
    } else {
        Err(McpError::UnsupportedTokenEndpointAuthenticationMethod)
    }
}

fn requested_scope(
    configured: &[String],
    metadata_scopes: &[String],
    previous_scope: Option<&str>,
    request_offline_access: bool,
) -> Result<Option<String>, McpError> {
    let mut tokens: Vec<&str> = Vec::new();
    append_scope_tokens(&mut tokens, previous_scope)?;
    let chosen = if configured.is_empty() {
        metadata_scopes
    } else {
        configured
    };
    for scope in chosen {
        append_scope_tokens(&mut tokens, Some(scope))?;
    }
    if request_offline_access {
        append_unique(&mut tokens, "offline_access")?;
    }
    Ok((!tokens.is_empty()).then(|| tokens.join(" ")))
}

fn append_scope_tokens<'a>(
    tokens: &mut Vec<&'a str>,
    scope: Option<&'a str>,
) -> Result<(), McpError> {
    for token in scope
        .unwrap_or_default()
        .split([' ', '\t', '\r', '\n'])
        .filter(|token| !token.is_empty())
    {
        append_unique(tokens, token)?;
    }
    Ok(())
}

fn append_unique<'a>(tokens: &mut Vec<&'a str>, value: &'a str) -> Result<(), McpError> {
    if value.is_empty() {
        return Ok(());
    }
    if value.len() > MAX_SCOPE_TOKEN_BYTES
        || !value
            .bytes()
            .all(|byte| matches!(byte, 0x21 | 0x23..=0x5b | 0x5d..=0x7e))
    {
        return Err(McpError::InvalidOAuthScope);
    }
    if tokens.contains(&value) {
        return Ok(());
    }
    if tokens.len() >= MAX_SCOPE_TOKENS {
        return Err(McpError::TooManyOAuthScopes);
    }
    tokens.push(value);
    Ok(())
}

fn authorization_url(
    endpoint: &str,
    fields: &[(&str, &str)],
    scope: Option<&str>,
) -> Zeroizing<String> {
    let mut form = FormBody::default();
    for (key, value) in fields {
        form.append(key, value);
    }
    if let Some(scope) = scope {
        form.append("scope", scope);
    }
    let separator = if endpoint.contains('?') { '&' } else { '?' };
    let mut url = Zeroizing::new(String::with_capacity(
        endpoint.len() + 1 + form.as_str().len(),
    ));
    let _ = write!(url, "{endpoint}{separator}{}", form.as_str());
    url
}

async fn wait_for_callback(
    listener: &CallbackListener,
    cancel: &CancellationToken,
) -> Result<AuthorizationResponse, McpError> {
    let classify: Classifier<AuthorizationResponse, McpError> = Arc::new(|target: &str| {
        if !target.starts_with("/callback?") {
            return ParseResult::Failed(McpError::InvalidAuthorizationCallback);
        }
        match parse_authorization_redirect(target) {
            Ok(response) => ParseResult::Accepted(response),
            Err(error) => ParseResult::Failed(error),
        }
    });
    let accepted = match tokio::time::timeout(CALLBACK_TIMEOUT, listener.accept(&classify, cancel))
        .await
    {
        Err(_) => return Err(McpError::McpAuthorizationCallbackTimedOut),
        Ok(Err(AwaitError::Cancelled)) => return Err(McpError::Cancelled),
        Ok(Err(AwaitError::ListenerFailed)) => return Err(McpError::InvalidAuthorizationCallback),
        Ok(Err(AwaitError::Rejected(error))) => return Err(error),
        Ok(Ok(accepted)) => accepted,
    };
    let response = accepted.callback.clone();
    accepted.respond(CallbackResponse::Ok).await?;
    Ok(response)
}

fn parse_authorization_redirect(location: &str) -> Result<AuthorizationResponse, McpError> {
    let (_, after) = location
        .split_once('?')
        .ok_or(McpError::InvalidAuthorizationRedirect)?;
    let query = after.split_once('#').map_or(after, |(query, _)| query);
    let code = query_value(query, "code").map_err(query_error)?;
    let state = query_value(query, "state").map_err(query_error)?;
    let issuer = match query_value(query, "iss") {
        Ok(value) => Some(value.to_string()),
        Err(QueryError::MissingQueryParameter) => None,
        Err(error) => return Err(query_error(error)),
    };
    Ok(AuthorizationResponse {
        code,
        state,
        issuer,
    })
}

fn query_error(error: QueryError) -> McpError {
    match error {
        QueryError::MissingQueryParameter => McpError::MissingQueryParameter,
        QueryError::InvalidPercentEncoding | QueryError::EmptyQueryValue => {
            McpError::InvalidPercentEncoding
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IssuerCheck {
    Matched,
    Mismatch,
}

fn validate_authorization_response(
    expected_state: &str,
    expected_issuer: &str,
    issuer_required: bool,
    response: &AuthorizationResponse,
) -> Result<IssuerCheck, McpError> {
    if expected_state != response.state.as_str() {
        return Err(McpError::OAuthStateMismatch);
    }
    match &response.issuer {
        None if issuer_required => Err(McpError::AuthorizationResponseIssuerMissing),
        Some(issuer) if issuer != expected_issuer => Ok(IssuerCheck::Mismatch),
        _ => Ok(IssuerCheck::Matched),
    }
}

struct CodeExchange<'a> {
    endpoint: String,
    resource: String,
    code: &'a str,
    verifier: &'a str,
    redirect_uri: &'a str,
    scope: Option<&'a str>,
}

async fn exchange_authorization_code(
    http: &reqwest::Client,
    metadata: AuthorizationMetadata,
    registration: ClientRegistration,
    exchange: CodeExchange<'_>,
) -> Result<Credentials, McpError> {
    let mut form = FormBody::default();
    form.append("grant_type", "authorization_code");
    form.append("code", exchange.code);
    form.append("redirect_uri", exchange.redirect_uri);
    form.append("code_verifier", exchange.verifier);
    form.append("resource", &exchange.resource);
    let authorization = token_endpoint_authentication(
        &mut form,
        &registration.token_endpoint_auth_method,
        &registration.client_id,
        registration.client_secret.as_deref().map(String::as_str),
    )?;
    let response = request(
        http,
        Method::POST,
        &metadata.token_endpoint,
        Payload::Form(&form),
        authorization.as_ref(),
    )
    .await?;
    if response.status != StatusCode::OK {
        return Err(McpError::TokenExchangeFailed);
    }
    validate_json_content_type(response.content_type.as_deref())?;
    let Value::Object(object) = parse_json(&response.body)? else {
        return Err(McpError::InvalidTokenResponse);
    };
    let access_token = required_secret(&object, "access_token")?;
    let refresh_token = optional_secret(&object, "refresh_token")?;
    let token_type = optional_string(&object, "token_type")?.unwrap_or_else(|| "Bearer".to_owned());
    if !token_type.eq_ignore_ascii_case("Bearer") {
        return Err(McpError::InvalidTokenResponse);
    }
    let scope = optional_string(&object, "scope")?
        .unwrap_or_else(|| exchange.scope.unwrap_or_default().to_owned());
    Ok(Credentials {
        endpoint: exchange.endpoint,
        resource: exchange.resource,
        issuer: metadata.issuer,
        client_id: registration.client_id,
        client_secret: registration.client_secret,
        access_token,
        refresh_token,
        scope,
        token_type,
        token_endpoint_auth_method: registration.token_endpoint_auth_method,
        expires_at_ms: token_expires_at(&object, now_ms())?,
        authorization_endpoint: metadata.authorization_endpoint,
        token_endpoint: metadata.token_endpoint,
        revocation_endpoint: metadata.revocation_endpoint,
    })
}

#[cfg(test)]
mod tests;
