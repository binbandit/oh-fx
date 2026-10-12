use std::time::Duration;

use ofx_contract::{
    ImageInputSupport, ModelCapabilities, is_valid_reasoning_effort, parse_strict_json_value,
};
use ofx_http::{ClientError, ConnectionOptions, build_connection_client};
use reqwest::StatusCode;
use serde_json::{Map, Value};
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
const MAX_REASONING_EFFORTS: usize = 16;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogView {
    Full,
    Picker,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayModel {
    pub id: String,
    released: i64,
    tool_use: bool,
    reasoning_efforts: Vec<String>,
    fast: bool,
    ultrafast: bool,
    image_input: ImageInputSupport,
    context_window: Option<u32>,
    max_tokens: Option<u32>,
}

impl GatewayModel {
    pub fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            reasoning_efforts: self.reasoning_efforts.clone(),
            supports_fast_mode: self.fast,
            supports_ultrafast_mode: self.ultrafast,
            context_window: self.context_window,
            image_input_support: self.image_input,
        }
    }

    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_tokens
    }
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
        view: CatalogView,
        cancel: &CancellationToken,
    ) -> CatalogFetch<GatewayModel> {
        let requested = AccessMetadata::of(access);
        let outcome = match self.fetch(access, view, cancel).await {
            Ok(models) => CatalogFetch::Loaded {
                models,
                access: requested,
                anonymous_fallback: false,
                fallback_failure: None,
            },
            Err(failure) => match access.public_fallback_after(failure) {
                Some(public) => {
                    let effective = AccessMetadata::of(&public);
                    match self.fetch(&public, view, cancel).await {
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
        view: CatalogView,
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
            Ok((StatusCode::OK, body)) => parse_catalog(&body, view).ok_or(CatalogFailed {
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

fn parse_catalog(body: &[u8], view: CatalogView) -> Option<Vec<GatewayModel>> {
    let Value::Object(root) = parse_strict_json_value(body).ok()? else {
        return None;
    };
    let Value::Array(entries) = root.get("data")? else {
        return None;
    };
    let mut models: Vec<GatewayModel> = entries.iter().filter_map(catalog_entry).collect();
    models.sort_by(order::catalog_order);
    Some(match view {
        CatalogView::Full => models,
        CatalogView::Picker => order::picker(models),
    })
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
    let tags = entry.get("tags");
    Some(GatewayModel {
        id: id.clone(),
        released: entry.get("released").and_then(Value::as_i64).unwrap_or(0),
        tool_use: tagged(tags, "tool-use"),
        reasoning_efforts: reasoning_efforts(entry.get("reasoning_options")),
        fast: supports_fast_mode(entry),
        ultrafast: supports_ultrafast_mode(entry),
        image_input: if tagged(tags, "vision") && tagged(tags, "file-input") {
            ImageInputSupport::Native
        } else {
            ImageInputSupport::NonNative
        },
        context_window: positive_limit(entry.get("context_window")),
        max_tokens: positive_limit(entry.get("max_tokens")),
    })
}

fn tagged(tags: Option<&Value>, tag: &str) -> bool {
    tags.and_then(Value::as_array).is_some_and(|tags| {
        tags.iter()
            .filter_map(Value::as_str)
            .any(|candidate| candidate.eq_ignore_ascii_case(tag))
    })
}

fn reasoning_efforts(options: Option<&Value>) -> Vec<String> {
    let mut efforts = Vec::new();
    let Some(options) = options.and_then(Value::as_array) else {
        return efforts;
    };
    let Some(values) = options
        .iter()
        .filter_map(Value::as_object)
        .filter(|option| option.get("type").and_then(Value::as_str) == Some("effort"))
        .find_map(|option| option.get("values").and_then(Value::as_array))
    else {
        return efforts;
    };
    for value in values.iter().filter_map(Value::as_str) {
        if efforts.len() >= MAX_REASONING_EFFORTS {
            break;
        }
        if is_valid_reasoning_effort(value) && !is_default_effort(value) {
            efforts.push(value.to_owned());
        }
    }
    efforts
}

fn is_default_effort(value: &str) -> bool {
    ["auto", "adaptive", "default"]
        .iter()
        .any(|default| value.eq_ignore_ascii_case(default))
}

fn supports_fast_mode(entry: &Map<String, Value>) -> bool {
    let toggled = entry
        .get("fast_options")
        .and_then(Value::as_array)
        .is_some_and(|options| {
            options
                .iter()
                .filter_map(Value::as_object)
                .any(|option| option.get("type").and_then(Value::as_str) == Some("toggle"))
        });
    let pricing = entry.get("pricing");
    let openai = entry
        .get("owned_by")
        .and_then(Value::as_str)
        .is_some_and(|owner| owner.eq_ignore_ascii_case("openai"));
    toggled
        || object_field(pricing, "fast").is_some_and(Value::is_object)
        || (openai
            && object_field(object_field(pricing, "service_tiers"), "priority")
                .is_some_and(Value::is_object))
}

fn supports_ultrafast_mode(entry: &Map<String, Value>) -> bool {
    let openai = entry
        .get("owned_by")
        .and_then(Value::as_str)
        .is_some_and(|owner| owner.eq_ignore_ascii_case("openai"));
    let pricing = entry.get("pricing");
    let Some(tier) = object_field(object_field(pricing, "service_tiers"), "ultrafast") else {
        return false;
    };
    let tier = Some(tier);
    openai
        && positive_price(object_field(tier, "input"))
        && positive_price(object_field(tier, "output"))
        && (object_field(pricing, "input_cache_read").is_none()
            || positive_price(object_field(tier, "input_cache_read")))
}

fn positive_price(value: Option<&Value>) -> bool {
    let price = match value {
        Some(Value::String(text)) => text.parse::<f64>().ok(),
        Some(Value::Number(number)) => number.as_f64(),
        _ => None,
    };
    price.is_some_and(|price| price.is_finite() && price > 0.0)
}

fn object_field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    value?.as_object()?.get(key)
}

fn positive_limit(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_i64)
        .filter(|limit| *limit > 0)
        .and_then(|limit| u32::try_from(limit).ok())
}

#[cfg(test)]
mod tests;
