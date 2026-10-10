use std::sync::Arc;

use serde_json::Value;
use tokio::time::Instant;

use crate::catalog_freshness::page_expiry;
use crate::error::McpError;
use crate::feature_catalog_runtime::FEATURE_RESPONSE_FRAME_CAP_BYTES;
use crate::features::common::ResourceContent;
use crate::features::completion::{
    self, CompletionArgument, CompletionReference, CompletionResult, parse_result, request_params,
};
use crate::features::prompts::{
    self, GetOutcome, Prompt, PromptArgument, PromptGetResult, parse_get_outcome,
    validate_arguments_json,
};
use crate::features::resources::{
    Limits, ReadOutcome, Resource, ResourceTemplate, parse_read_outcome, stale_fallback_eligible,
};
use crate::mcp_contract::TransportType;
use crate::operation_control::monotonic_millis;
use crate::protocol_messages::{
    build_completion_request, build_prompt_get_request, build_resource_read_request,
};
use crate::server_connection::McpClient;
use crate::server_lifecycle::{Lifecycle, RestartFailure, Server};
use crate::tool_result::protocol_diagnostic;
use crate::transport::{McpTransport, TransportRequest};

#[derive(Debug, Clone, PartialEq)]
pub enum FeatureFailure {
    Error(McpError),
    Diagnostic(String),
}

impl From<McpError> for FeatureFailure {
    fn from(error: McpError) -> Self {
        Self::Error(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSummary {
    pub identity: String,
    pub name: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSummary {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub arguments: Vec<PromptArgument>,
}

impl Server {
    pub(crate) async fn list_resources(
        self: &Arc<Self>,
        include_templates: bool,
        deadline: Instant,
    ) -> Result<Vec<ResourceSummary>, McpError> {
        if !self.features.advertises_resources() {
            return Err(McpError::McpResourcesUnsupported);
        }
        let resources = self.feature_catalog::<Resource>(deadline).await?;
        if !include_templates {
            return Ok(resources
                .iter()
                .map(|resource| ResourceSummary {
                    identity: resource.uri.clone(),
                    name: resource.name.clone(),
                    title: resource.title.clone(),
                })
                .collect());
        }
        let templates = self.feature_catalog::<ResourceTemplate>(deadline).await?;
        Ok(templates
            .iter()
            .map(|template| ResourceSummary {
                identity: template.uri_template.clone(),
                name: template.name.clone(),
                title: template.title.clone(),
            })
            .collect())
    }

    pub(crate) async fn list_prompts(
        self: &Arc<Self>,
        deadline: Instant,
    ) -> Result<Vec<PromptSummary>, McpError> {
        if !self.features.advertises_prompts() {
            return Err(McpError::McpPromptsUnsupported);
        }
        let prompts = self.feature_catalog::<Prompt>(deadline).await?;
        Ok(prompts
            .iter()
            .map(|prompt| PromptSummary {
                name: prompt.name.clone(),
                title: prompt.title.clone(),
                description: prompt.description.clone(),
                arguments: prompt.arguments.clone(),
            })
            .collect())
    }

    pub(crate) async fn read_resource(
        self: &Arc<Self>,
        uri: &str,
        deadline: Instant,
    ) -> Result<Arc<[ResourceContent]>, FeatureFailure> {
        if !self.features.advertises_resources() {
            return Err(McpError::McpResourcesUnsupported.into());
        }
        loop {
            let identity = self.resource_identity(uri, deadline).await?;
            let client = match self.lifecycle() {
                Lifecycle::Ready(client) => Some(client),
                Lifecycle::Idle | Lifecycle::Starting | Lifecycle::Failed(_) => None,
            };
            if client
                .as_ref()
                .is_some_and(|client| client.resources_invalidation.pending())
            {
                self.features.clear_reads();
            }
            let now_ms = monotonic_millis();
            if let Some(cached) = self.features.cached_read(uri, now_ms, false) {
                return Ok(cached);
            }
            let stale = self.features.cached_read(uri, now_ms, true);
            let fall_back = |error: McpError, eligible: bool| match &stale {
                Some(stale) if eligible => Ok(Arc::clone(stale)),
                _ => Err(FeatureFailure::Error(error)),
            };
            let Some(client) = client else {
                return fall_back(McpError::McpConnectionClosed, true);
            };
            if self.config.transport == TransportType::Stdio && !client.is_running() {
                match self.running_client(deadline).await {
                    Ok(_) => continue,
                    Err(failure) => {
                        let error = failure.into_error();
                        let eligible = stale_fallback_eligible(&error)
                            || matches!(error, McpError::McpRestartLimitReached);
                        return fall_back(error, eligible);
                    }
                }
            }
            let current = self
                .while_current(&client, || identity.current(&self.features))
                .unwrap_or(false);
            if !current || client.resources_invalidation.pending() {
                return Err(McpError::McpFeatureCatalogChanged.into());
            }
            let (outcome, received_at_ms) = match request_read(&client, uri, deadline).await {
                Ok(received) => received,
                Err(error) => {
                    let eligible = stale_fallback_eligible(&error);
                    return fall_back(error, eligible);
                }
            };
            let result = match outcome {
                ReadOutcome::Complete(result) => result,
                ReadOutcome::ProtocolFailure(error) => {
                    return Err(FeatureFailure::Diagnostic(protocol_diagnostic(&error)));
                }
            };
            let contents: Arc<[ResourceContent]> = result.contents.into();
            let expires_at_ms = page_expiry(received_at_ms, result.cache.ttl_ms);
            self.while_current(&client, || {
                if identity.current(&self.features) && !client.resources_invalidation.pending() {
                    self.features
                        .publish_read(uri, Arc::clone(&contents), expires_at_ms);
                }
            });
            return Ok(contents);
        }
    }

    pub(crate) async fn get_prompt(
        self: &Arc<Self>,
        name: &str,
        arguments_json: &str,
        deadline: Instant,
    ) -> Result<PromptGetResult, FeatureFailure> {
        if !self.features.advertises_prompts() {
            return Err(McpError::McpPromptsUnsupported.into());
        }
        loop {
            let (identity, arguments) = self
                .prompt_identity(name, deadline, |prompt| {
                    validate_arguments_json(prompt, arguments_json, prompts::Limits::default())
                })
                .await?;
            let Some(client) = self.feature_client(deadline).await? else {
                continue;
            };
            self.check_current(&client, &identity)?;
            return match request_prompt(&client, name, &arguments, deadline).await? {
                GetOutcome::Complete(result) => Ok(result),
                GetOutcome::ProtocolFailure(error) => {
                    Err(FeatureFailure::Diagnostic(protocol_diagnostic(&error)))
                }
            };
        }
    }

    pub(crate) async fn complete(
        self: &Arc<Self>,
        reference: CompletionReference<'_>,
        argument: CompletionArgument<'_>,
        context: &[CompletionArgument<'_>],
        deadline: Instant,
    ) -> Result<CompletionResult, McpError> {
        if !self.features.advertises_completion() {
            return Err(McpError::McpCompletionUnsupported);
        }
        match reference {
            CompletionReference::Prompt(_) if !self.features.advertises_prompts() => {
                return Err(McpError::McpPromptsUnsupported);
            }
            CompletionReference::ResourceTemplate(_) if !self.features.advertises_resources() => {
                return Err(McpError::McpResourcesUnsupported);
            }
            CompletionReference::Prompt(_) | CompletionReference::ResourceTemplate(_) => {}
        }
        loop {
            let identity = match reference {
                CompletionReference::Prompt(name) => {
                    self.prompt_identity(name, deadline, |_| Ok(())).await?.0
                }
                CompletionReference::ResourceTemplate(uri_template) => {
                    self.template_identity(uri_template, deadline).await?
                }
            };
            let Some(client) = self.feature_client(deadline).await? else {
                continue;
            };
            let params =
                request_params(reference, argument, context, completion::Limits::default())?;
            self.check_current(&client, &identity)?;
            let id = client.transport.next_request_id()?;
            let response = client
                .transport
                .request(TransportRequest::new(
                    id,
                    build_completion_request(id, &params),
                    FEATURE_RESPONSE_FRAME_CAP_BYTES,
                    deadline,
                ))
                .await?;
            return parse_result(&response, completion::Limits::default());
        }
    }

    async fn feature_client(
        self: &Arc<Self>,
        deadline: Instant,
    ) -> Result<Option<Arc<McpClient>>, McpError> {
        let Lifecycle::Ready(client) = self.lifecycle() else {
            return Err(McpError::McpConnectionClosed);
        };
        if self.config.transport == TransportType::Stdio && !client.is_running() {
            self.running_client(deadline)
                .await
                .map_err(RestartFailure::into_error)?;
            return Ok(None);
        }
        Ok(Some(client))
    }
}

async fn request_prompt(
    client: &McpClient,
    name: &str,
    arguments: &Value,
    deadline: Instant,
) -> Result<GetOutcome, McpError> {
    let id = client.transport.next_request_id()?;
    let response = client
        .transport
        .request(TransportRequest::new(
            id,
            build_prompt_get_request(id, name, arguments),
            FEATURE_RESPONSE_FRAME_CAP_BYTES,
            deadline,
        ))
        .await?;
    parse_get_outcome(&response, prompts::Limits::default())
}

async fn request_read(
    client: &McpClient,
    uri: &str,
    deadline: Instant,
) -> Result<(ReadOutcome, u64), McpError> {
    let id = client.transport.next_request_id()?;
    let response = client
        .transport
        .request(TransportRequest::new(
            id,
            build_resource_read_request(id, uri),
            FEATURE_RESPONSE_FRAME_CAP_BYTES,
            deadline,
        ))
        .await?;
    let received_at_ms = monotonic_millis();
    Ok((
        parse_read_outcome(&response, Limits::default())?,
        received_at_ms,
    ))
}
