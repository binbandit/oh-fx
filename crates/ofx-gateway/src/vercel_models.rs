use std::time::Duration;

use ofx_contract::parse_strict_json_value;
use ofx_http::{ClientError, ConnectionOptions, build_connection_client};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{BoundedFailure, bounded_get};
use crate::model_catalog::{
    AccessMetadata, CatalogAccess, CatalogFailed, CatalogFailure, CatalogFetch, trace_load,
};

mod order;

pub const GATEWAY_MODELS_PATH: &str = "/coding-agent/v1/models";
const MODELS_URL: &str = "https://ai-gateway.vercel.sh/coding-agent/v1/models";
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayModelsEndpoints {
    pub models: String,
}

impl Default for GatewayModelsEndpoints {
    fn default() -> Self {
        Self {
            models: MODELS_URL.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayModel {
    pub id: String,
    released: i64,
    tool_use: bool,
}

pub struct GatewayModelCatalog {
    client: reqwest::Client,
    url: String,
}

impl GatewayModelCatalog {
    pub fn new(user_agent: &str, endpoints: GatewayModelsEndpoints) -> Result<Self, ClientError> {
        let client = build_connection_client(&ConnectionOptions {
            user_agent: user_agent.to_owned(),
            follow_redirects: false,
            ..ConnectionOptions::default()
        })?;
        Ok(Self {
            client,
            url: endpoints.models,
        })
    }

    pub async fn load(
        &self,
        access: &CatalogAccess,
        cancel: &CancellationToken,
    ) -> CatalogFetch<GatewayModel> {
        let requested = AccessMetadata::of(access);
        let outcome = match self.fetch(access, cancel).await {
            Ok(models) => CatalogFetch::Loaded {
                models,
                access: requested,
                anonymous_fallback: false,
                fallback_failure: None,
            },
            Err(failure) => match access.public_fallback_after(failure) {
                Some(public) => {
                    let effective = AccessMetadata::of(&public);
                    match self.fetch(&public, cancel).await {
                        Ok(models) => CatalogFetch::Loaded {
                            models,
                            access: effective,
                            anonymous_fallback: true,
                            fallback_failure: Some(failure),
                        },
                        Err(failure) => CatalogFetch::Failed {
                            access: effective,
                            anonymous_fallback: true,
                            failure,
                        },
                    }
                }
                None => CatalogFetch::Failed {
                    access: requested,
                    anonymous_fallback: false,
                    failure,
                },
            },
        };
        trace_load(requested, &outcome);
        outcome
    }

    async fn fetch(
        &self,
        access: &CatalogAccess,
        cancel: &CancellationToken,
    ) -> Result<Vec<GatewayModel>, CatalogFailed> {
        let mut request = match access.login_team() {
            Some(team) => {
                let separator = if self.url.contains('?') { '&' } else { '?' };
                self.client
                    .get(format!("{}{separator}teamId={team}", self.url))
            }
            None => self.client.get(&self.url),
        };
        if let Some(credential) = access.credential() {
            request = request.bearer_auth(credential);
        }
        let deadline = Instant::now() + FETCH_TIMEOUT;
        match bounded_get(request, MAX_CATALOG_BYTES, deadline, cancel).await {
            Ok((StatusCode::OK, body)) => parse_catalog(&body).ok_or(CatalogFailed {
                failure: CatalogFailure::MalformedResponse,
                http_status: Some(StatusCode::OK.as_u16()),
                retryable: false,
            }),
            Ok((status, _)) => Err(CatalogFailed::for_http_status(status.as_u16())),
            Err(BoundedFailure::Cancelled) => Err(CatalogFailed::without_status(
                CatalogFailure::Cancellation,
                false,
            )),
            Err(failure) => Err(CatalogFailed::without_status(
                CatalogFailure::Transport,
                failure.retryable(),
            )),
        }
    }
}

fn parse_catalog(body: &[u8]) -> Option<Vec<GatewayModel>> {
    let Value::Object(root) = parse_strict_json_value(body).ok()? else {
        return None;
    };
    let Value::Array(entries) = root.get("data")? else {
        return None;
    };
    let mut models: Vec<GatewayModel> = entries.iter().filter_map(catalog_entry).collect();
    models.sort_by(order::catalog_order);
    Some(models)
}

fn catalog_entry(entry: &Value) -> Option<GatewayModel> {
    let Value::Object(entry) = entry else {
        return None;
    };
    if let Some(Value::String(kind)) = entry.get("type")
        && !kind.eq_ignore_ascii_case("language")
    {
        return None;
    }
    let Value::String(id) = entry.get("id")? else {
        return None;
    };
    Some(GatewayModel {
        id: id.clone(),
        released: entry.get("released").and_then(Value::as_i64).unwrap_or(0),
        tool_use: tagged(entry.get("tags"), "tool-use"),
    })
}

fn tagged(tags: Option<&Value>, tag: &str) -> bool {
    tags.and_then(Value::as_array).is_some_and(|tags| {
        tags.iter()
            .filter_map(Value::as_str)
            .any(|candidate| candidate.eq_ignore_ascii_case(tag))
    })
}

#[cfg(test)]
mod tests;
