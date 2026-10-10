use std::sync::Arc;

use tokio::time::Instant;

use crate::error::McpError;
use crate::features::prompts::{Prompt, PromptArgument};
use crate::features::resources::{Resource, ResourceTemplate};
use crate::server_lifecycle::Server;

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
}
