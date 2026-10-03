use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use ofx_contract::parse_strict_json_value;
use ofx_contract::{CODEX_ORIGINATOR, ModelCapabilities, is_valid_reasoning_effort};
use ofx_http::{ClientError, ConnectionOptions, build_connection_client};
use reqwest::StatusCode;
use reqwest::header::ACCEPT;
use serde_json::{Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::client::{BoundedFailure, bounded_get};
use crate::model_catalog::{CatalogFailure, failure_for_http_status};
use crate::provider_versions::{Version, VersionError, VersionLookup};

const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const CLIENT_VERSION_URL: &str = "https://registry.npmjs.org/@openai/codex/latest";
const MAX_CATALOG_MODELS: usize = 128;
const MAX_MODEL_ID_BYTES: usize = 1024;
const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_REASONING_EFFORTS: usize = 16;
const MAX_LISTED_VALUES: usize = 32;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
pub const CODEX_TITLE_MODEL: &str = "gpt-5.6-luna";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexModelsEndpoints {
    pub models: String,
    pub client_version: String,
}

impl Default for CodexModelsEndpoints {
    fn default() -> Self {
        Self {
            models: MODELS_URL.to_owned(),
            client_version: CLIENT_VERSION_URL.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexModel {
    pub id: String,
    pub capabilities: ModelCapabilities,
}

pub struct CatalogCredential {
    token: Zeroizing<String>,
    account_id: String,
}

impl CatalogCredential {
    pub fn new(token: String, account_id: String) -> Self {
        Self {
            token: Zeroizing::new(token),
            account_id,
        }
    }
}

impl fmt::Debug for CatalogCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogCredential")
            .field("token", &"<redacted>")
            .field("account_id", &"<redacted>")
            .finish()
    }
}

#[derive(Debug)]
pub struct CodexModelCatalog {
    client: reqwest::Client,
    endpoints: CodexModelsEndpoints,
    cache_directory: Option<PathBuf>,
}

impl CodexModelCatalog {
    pub fn new(
        user_agent: &str,
        endpoints: CodexModelsEndpoints,
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
        credential: Option<&CatalogCredential>,
        cancel: &CancellationToken,
    ) -> Result<Vec<CodexModel>, CatalogFailure> {
        let deadline = Instant::now() + FETCH_TIMEOUT;
        let version = match credential {
            Some(_) => Some(self.client_version(cancel, deadline).await?),
            None => None,
        };
        let mut request = self
            .client
            .get(models_url(&self.endpoints.models, version.as_ref()))
            .header("originator", CODEX_ORIGINATOR)
            .header(ACCEPT, "application/json");
        if let Some(credential) = credential {
            request = request
                .bearer_auth(credential.token.as_str())
                .header("chatgpt-account-id", credential.account_id.as_str());
        }
        match bounded_get(request, MAX_CATALOG_BYTES, deadline, cancel).await {
            Ok((StatusCode::OK, body)) => {
                parse_catalog(&body).ok_or(CatalogFailure::MalformedResponse)
            }
            Ok((status, _)) => Err(failure_for_http_status(status.as_u16())),
            Err(BoundedFailure::Cancelled) => Err(CatalogFailure::Cancellation),
            Err(BoundedFailure::Failed | BoundedFailure::TooLarge) => {
                Err(CatalogFailure::Transport)
            }
        }
    }

    async fn client_version(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Version, CatalogFailure> {
        let lookup = VersionLookup {
            client: &self.client,
            url: &self.endpoints.client_version,
            cache_directory: self.cache_directory.as_deref(),
        };
        lookup
            .resolve(cancel, deadline)
            .await
            .map_err(|error| match error {
                VersionError::Cancelled => CatalogFailure::Cancellation,
                VersionError::Unavailable => CatalogFailure::Transport,
            })
    }
}

fn models_url(base: &str, version: Option<&Version>) -> String {
    let Some(version) = version else {
        return base.to_owned();
    };
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}client_version={}", version.as_str())
}

fn parse_catalog(body: &[u8]) -> Option<Vec<CodexModel>> {
    let Value::Object(root) = parse_strict_json_value(body).ok()? else {
        return None;
    };
    let Value::Array(models) = root.get("models")? else {
        return None;
    };
    if models.len() > MAX_CATALOG_MODELS {
        return None;
    }
    let mut listed_models = Vec::new();
    for model in models {
        let Value::Object(model) = model else {
            return None;
        };
        if listed(model)? {
            listed_models.push(listed_model(model)?);
        }
    }
    Some(listed_models)
}

fn listed(model: &Map<String, Value>) -> Option<bool> {
    let visibility = required_string(model, "visibility")?;
    let supported = model.get("supported_in_api")?.as_bool()?;
    Some(supported && visibility == "list")
}

fn listed_model(model: &Map<String, Value>) -> Option<CodexModel> {
    let slug = required_string(model, "slug").filter(|slug| valid_model_id(slug))?;
    let reasoning_efforts = reasoning_levels(model)?;
    let context_window = match model.get("context_window") {
        None | Some(Value::Null) => None,
        Some(window) => Some(u32::try_from(window.as_u64()?).ok()?).filter(|window| *window > 0),
    };
    lists_value(model, "input_modalities", "image")?;
    let supports_fast_mode = lists_value(model, "additional_speed_tiers", "fast")?;
    Some(CodexModel {
        id: slug.to_owned(),
        capabilities: ModelCapabilities {
            reasoning_efforts,
            supports_fast_mode,
            context_window,
        },
    })
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key)?.as_str().filter(|value| !value.is_empty())
}

fn valid_model_id(id: &str) -> bool {
    (1..=MAX_MODEL_ID_BYTES).contains(&id.len())
        && id.bytes().all(|byte| byte > 0x20 && byte != 0x7f)
}

fn reasoning_levels(model: &Map<String, Value>) -> Option<Vec<String>> {
    let Some(levels) = model.get("supported_reasoning_levels") else {
        return Some(Vec::new());
    };
    let Value::Array(levels) = levels else {
        return None;
    };
    if levels.len() > MAX_REASONING_EFFORTS {
        return None;
    }
    levels
        .iter()
        .map(|level| {
            let effort = required_string(level.as_object()?, "effort")?;
            is_valid_reasoning_effort(effort).then(|| effort.to_owned())
        })
        .collect()
}

fn lists_value(model: &Map<String, Value>, key: &str, expected: &str) -> Option<bool> {
    let Some(values) = model.get(key) else {
        return Some(false);
    };
    let Value::Array(values) = values else {
        return None;
    };
    if values.len() > MAX_LISTED_VALUES {
        return None;
    }
    for value in values {
        if value.as_str()? == expected {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(test)]
mod tests;
