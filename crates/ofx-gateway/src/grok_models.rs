use std::path::PathBuf;
use std::time::Duration;

use ofx_config::parse_strict_json;
use ofx_contract::{is_valid_reasoning_effort, valid_credential_account_id};
use ofx_http::{ClientError, ConnectionOptions, build_connection_client};
use reqwest::StatusCode;
use serde_json::{Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{BoundedFailure, bounded_get};
use crate::model_catalog::{CatalogFailure, failure_for_http_status};
use crate::openai_codex_models::CatalogCredential;
use crate::provider_versions::{VersionError, VersionLookup};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokModel {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokModelsEndpoints {
    pub models: String,
    pub modalities: String,
    pub client_version: String,
}

impl Default for GrokModelsEndpoints {
    fn default() -> Self {
        Self {
            models: "https://cli-chat-proxy.grok.com/v1/models".to_owned(),
            modalities: "https://api.x.ai/v1/language-models".to_owned(),
            client_version: "https://x.ai/cli/stable".to_owned(),
        }
    }
}

pub struct GrokModelCatalog {
    client: reqwest::Client,
    endpoints: GrokModelsEndpoints,
    cache_directory: Option<PathBuf>,
}

impl GrokModelCatalog {
    pub fn new(
        user_agent: &str,
        endpoints: GrokModelsEndpoints,
        cache_directory: Option<PathBuf>,
    ) -> Result<Self, ClientError> {
        let client = build_connection_client(&ConnectionOptions {
            user_agent: user_agent.to_owned(),
            follow_redirects: false,
            ..ConnectionOptions::default()
        })?;
        Ok(Self {
            client,
            endpoints,
            cache_directory,
        })
    }

    pub async fn fetch(
        &self,
        credential: &CatalogCredential,
        cancel: &CancellationToken,
    ) -> Result<Vec<GrokModel>, CatalogFailure> {
        let token = credential.token();
        let account = credential.account_id();
        if token.is_empty() || !valid_credential_account_id(account) {
            return Err(CatalogFailure::Authentication);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let version = VersionLookup {
            client: &self.client,
            url: &self.endpoints.client_version,
            cache_directory: self.cache_directory.as_deref(),
        }
        .resolve_grok(cancel, deadline)
        .await
        .map_err(|error| match error {
            VersionError::Cancelled => CatalogFailure::Cancellation,
            VersionError::Unavailable => CatalogFailure::Transport,
        })?;
        let request = self
            .client
            .get(&self.endpoints.models)
            .header("accept", "application/json")
            .bearer_auth(token)
            .header("x-userid", account)
            .header("x-xai-token-auth", "xai-grok-cli")
            .header("x-grok-client-version", version.as_str())
            .header("x-grok-client-identifier", "fx");
        let (status, body) = catalog_get(request, deadline, cancel).await?;
        if status != StatusCode::OK {
            return Err(failure_for_http_status(status.as_u16()));
        }

        let request = self
            .client
            .get(&self.endpoints.modalities)
            .header("accept", "application/json")
            .bearer_auth(token);
        if let Err(CatalogFailure::Cancellation) = catalog_get(request, deadline, cancel).await {
            return Err(CatalogFailure::Cancellation);
        }
        parse_catalog(&body)
    }
}

async fn catalog_get(
    request: reqwest::RequestBuilder,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<(StatusCode, Vec<u8>), CatalogFailure> {
    bounded_get(request, 1024 * 1024, deadline, cancel)
        .await
        .map_err(|error| match error {
            BoundedFailure::Cancelled => CatalogFailure::Cancellation,
            BoundedFailure::Failed => CatalogFailure::Transport,
            BoundedFailure::TooLarge => CatalogFailure::MalformedResponse,
        })
}

fn parse_catalog(body: &[u8]) -> Result<Vec<GrokModel>, CatalogFailure> {
    let invalid = CatalogFailure::MalformedResponse;
    if body.len() > 1024 * 1024 {
        return Err(invalid);
    }
    let root = parse_strict_json(body).map_err(|_| invalid)?;
    let rows = root
        .as_object()
        .and_then(|root| root.get("data"))
        .and_then(Value::as_array)
        .filter(|rows| rows.len() <= 128)
        .ok_or(invalid)?;
    let mut models = Vec::new();
    for row in rows {
        let object = row.as_object().ok_or(invalid)?;
        let backend = required_string(object, "api_backend")?;
        if backend != "responses" {
            continue;
        }
        let id = required_string(object, "model")?;
        if id.len() > 256 || id.bytes().any(|byte| byte <= 0x20 || byte == 0x7f) {
            return Err(invalid);
        }
        positive_u32(object.get("context_window").ok_or(invalid)?)?;
        if let Some(value) = object
            .get("max_completion_tokens")
            .filter(|value| !value.is_null())
        {
            positive_u32(value)?;
        }
        let supported = object
            .get("supports_reasoning_effort")
            .and_then(Value::as_bool)
            .ok_or(invalid)?;
        let efforts = object
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .filter(|values| values.len() <= 16)
            .ok_or(invalid)?;
        let mut reasoning_efforts = Vec::new();
        for value in efforts {
            let effort = required_string(value.as_object().ok_or(invalid)?, "value")?;
            if effort == "auto"
                || !is_valid_reasoning_effort(effort)
                || reasoning_efforts.iter().any(|prior| prior == effort)
            {
                return Err(invalid);
            }
            reasoning_efforts.push(effort.to_owned());
        }
        if supported == reasoning_efforts.is_empty() {
            return Err(invalid);
        }
        models.push(GrokModel { id: id.to_owned() });
    }
    Ok(models)
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a str, CatalogFailure> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(CatalogFailure::MalformedResponse)
}

fn positive_u32(value: &Value) -> Result<u32, CatalogFailure> {
    value
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .filter(|number| *number > 0)
        .ok_or(CatalogFailure::MalformedResponse)
}

#[cfg(test)]
mod tests;
