use reqwest::{Method, StatusCode};
use serde_json::{Map, Value};

use super::{Payload, parse_json, request, validate_json_content_type};
use crate::error::McpError;
use crate::oauth_uri::{OAuthUri, canonical_resource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceMetadata {
    pub(crate) resource: String,
    pub(crate) authorization_servers: Vec<String>,
    pub(crate) scopes_supported: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthorizationMetadata {
    pub(crate) issuer: String,
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    pub(crate) registration_endpoint: Option<String>,
    pub(crate) revocation_endpoint: Option<String>,
    pub(crate) scopes_supported: Vec<String>,
    pub(crate) grant_types_supported: Vec<String>,
    pub(crate) token_endpoint_auth_methods_supported: Vec<String>,
    pub(crate) code_challenge_methods_supported: Vec<String>,
    pub(crate) client_id_metadata_document_supported: bool,
    pub(crate) authorization_response_iss_parameter_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MetadataOutcome {
    Metadata(Box<AuthorizationMetadata>),
    IssuerMismatch,
}

impl AuthorizationMetadata {
    pub(crate) fn supports_s256(&self) -> bool {
        contains(&self.code_challenge_methods_supported, "S256")
    }

    pub(crate) fn supports_refresh_token(&self) -> bool {
        contains(&self.grant_types_supported, "refresh_token")
    }

    pub(crate) fn supports_method(&self, method: &str) -> bool {
        contains(&self.token_endpoint_auth_methods_supported, method)
    }
}

pub(crate) fn contains(values: &[String], needle: &str) -> bool {
    values.iter().any(|value| value == needle)
}

pub(crate) fn protected_resource_metadata_urls(resource: &str) -> Result<Vec<String>, McpError> {
    let uri = OAuthUri::parse(resource).ok_or(McpError::InvalidMcpAuthEndpoint)?;
    let origin = uri.origin()?;
    let path = uri.raw_path();
    let mut urls = Vec::with_capacity(2);
    if path != "/" {
        let separator = if path.starts_with('/') { "" } else { "/" };
        urls.push(format!(
            "{origin}/.well-known/oauth-protected-resource{separator}{path}"
        ));
    }
    urls.push(format!("{origin}/.well-known/oauth-protected-resource"));
    Ok(urls)
}

pub(crate) fn authorization_metadata_urls(issuer: &str) -> Result<Vec<String>, McpError> {
    let uri = OAuthUri::parse(issuer).ok_or(McpError::InvalidAuthorizationIssuer)?;
    if !uri.is_secure_or_loopback() || has_query(issuer) || uri.fragment.is_some() {
        return Err(McpError::InvalidAuthorizationIssuer);
    }
    let origin = uri.origin()?;
    let path = uri.raw_path().trim_matches('/');
    Ok(if path.is_empty() {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server/{path}"),
            format!("{origin}/.well-known/openid-configuration/{path}"),
            format!("{issuer}/.well-known/openid-configuration"),
        ]
    })
}

fn has_query(text: &str) -> bool {
    text.split('#')
        .next()
        .is_some_and(|text| text.contains('?'))
}

pub(crate) fn parse_resource_metadata(
    bytes: &[u8],
    expected_resource: &str,
) -> Result<ResourceMetadata, McpError> {
    let Value::Object(object) = parse_json(bytes)? else {
        return Err(McpError::InvalidProtectedResourceMetadata);
    };
    let resource = canonical_resource(required_string(&object, "resource")?)?;
    if !resource_covers_endpoint(&resource, expected_resource) {
        return Err(McpError::McpAuthResourceMismatch);
    }
    let authorization_servers = required_string_array(&object, "authorization_servers")?;
    if authorization_servers.is_empty() {
        return Err(McpError::InvalidProtectedResourceMetadata);
    }
    Ok(ResourceMetadata {
        resource,
        authorization_servers,
        scopes_supported: optional_string_array(&object, "scopes_supported")?,
    })
}

pub(crate) fn parse_authorization_metadata(
    bytes: &[u8],
    expected_issuer: &str,
) -> Result<MetadataOutcome, McpError> {
    let Value::Object(object) = parse_json(bytes)? else {
        return Err(McpError::InvalidAuthorizationMetadata);
    };
    let issuer = required_string(&object, "issuer")?;
    if without_trailing_slash(issuer) != without_trailing_slash(expected_issuer) {
        return Ok(MetadataOutcome::IssuerMismatch);
    }
    Ok(MetadataOutcome::Metadata(Box::new(AuthorizationMetadata {
        authorization_endpoint: required_url(&object, "authorization_endpoint")?,
        token_endpoint: required_url(&object, "token_endpoint")?,
        registration_endpoint: optional_url(&object, "registration_endpoint")?,
        revocation_endpoint: optional_url(&object, "revocation_endpoint")?,
        scopes_supported: optional_string_array(&object, "scopes_supported")?,
        grant_types_supported: optional_string_array(&object, "grant_types_supported")?,
        token_endpoint_auth_methods_supported: match object
            .get("token_endpoint_auth_methods_supported")
        {
            Some(value) => string_array(value)?,
            None => vec!["client_secret_basic".to_owned()],
        },
        code_challenge_methods_supported: optional_string_array(
            &object,
            "code_challenge_methods_supported",
        )?,
        client_id_metadata_document_supported: object
            .get("client_id_metadata_document_supported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        authorization_response_iss_parameter_supported: object
            .get("authorization_response_iss_parameter_supported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        issuer: issuer.to_owned(),
    })))
}

fn without_trailing_slash(issuer: &str) -> &str {
    if issuer.len() > 1 {
        issuer.strip_suffix('/').unwrap_or(issuer)
    } else {
        issuer
    }
}

pub(crate) async fn discover_resource_metadata(
    http: &reqwest::Client,
    resource: &str,
) -> Result<ResourceMetadata, McpError> {
    for url in protected_resource_metadata_urls(resource)? {
        let Ok(response) = request(http, Method::GET, &url, Payload::Empty, None).await else {
            continue;
        };
        if response.status != StatusCode::OK {
            continue;
        }
        validate_json_content_type(response.content_type.as_deref())?;
        return parse_resource_metadata(&response.body, resource);
    }
    Err(McpError::ProtectedResourceMetadataUnavailable)
}

pub(crate) async fn discover_authorization_metadata(
    http: &reqwest::Client,
    issuer: &str,
) -> Result<MetadataOutcome, McpError> {
    for url in authorization_metadata_urls(issuer)? {
        let Ok(response) = request(http, Method::GET, &url, Payload::Empty, None).await else {
            continue;
        };
        if response.status != StatusCode::OK {
            continue;
        }
        validate_json_content_type(response.content_type.as_deref())?;
        return parse_authorization_metadata(&response.body, issuer);
    }
    Err(McpError::AuthorizationMetadataUnavailable)
}

pub(crate) fn validate_authorization_metadata_urls(
    metadata: &AuthorizationMetadata,
    resource: &str,
) -> Result<(), McpError> {
    validate_oauth_url_for_resource(&metadata.authorization_endpoint, resource)?;
    validate_oauth_url_for_resource(&metadata.token_endpoint, resource)?;
    for url in [
        &metadata.registration_endpoint,
        &metadata.revocation_endpoint,
    ]
    .into_iter()
    .flatten()
    {
        validate_oauth_url_for_resource(url, resource)?;
    }
    Ok(())
}

pub(crate) fn validate_oauth_url_for_resource(
    candidate: &str,
    resource: &str,
) -> Result<(), McpError> {
    let candidate = OAuthUri::parse(candidate).ok_or(McpError::InvalidMcpAuthEndpoint)?;
    if candidate.has_userinfo || candidate.fragment.is_some() {
        return Err(McpError::InsecureMcpAuthEndpoint);
    }
    if candidate.scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    let resource = OAuthUri::parse(resource).ok_or(McpError::InvalidMcpAuthEndpoint)?;
    if candidate.is_loopback() && resource.has_loopback_host() {
        Ok(())
    } else {
        Err(McpError::InsecureMcpAuthEndpoint)
    }
}

fn resource_covers_endpoint(resource: &str, endpoint: &str) -> bool {
    if resource == endpoint {
        return true;
    }
    let Some(rest) = endpoint.strip_prefix(resource) else {
        return false;
    };
    resource.is_empty() || resource.ends_with('/') || rest.starts_with('/')
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, McpError> {
    match object.get(key) {
        None => Err(McpError::MissingMetadataField),
        Some(Value::String(value)) if !value.is_empty() => Ok(value),
        Some(_) => Err(McpError::InvalidMetadataField),
    }
}

fn required_url(object: &Map<String, Value>, key: &str) -> Result<String, McpError> {
    let value = required_string(object, key)?;
    metadata_url(value)
}

fn optional_url(object: &Map<String, Value>, key: &str) -> Result<Option<String>, McpError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => metadata_url(value).map(Some),
        Some(_) => Err(McpError::InvalidMetadataUrl),
    }
}

fn metadata_url(value: &str) -> Result<String, McpError> {
    match OAuthUri::parse(value) {
        Some(uri) if uri.is_secure_or_loopback() && uri.fragment.is_none() => Ok(value.to_owned()),
        _ => Err(McpError::InvalidMetadataUrl),
    }
}

fn required_string_array(object: &Map<String, Value>, key: &str) -> Result<Vec<String>, McpError> {
    string_array(object.get(key).ok_or(McpError::MissingMetadataField)?)
}

fn optional_string_array(object: &Map<String, Value>, key: &str) -> Result<Vec<String>, McpError> {
    object.get(key).map_or(Ok(Vec::new()), string_array)
}

fn string_array(value: &Value) -> Result<Vec<String>, McpError> {
    let Value::Array(items) = value else {
        return Err(McpError::InvalidMetadataField);
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(text) if !text.is_empty() => Ok(text.clone()),
            _ => Err(McpError::InvalidMetadataField),
        })
        .collect()
}

#[cfg(test)]
mod tests;
