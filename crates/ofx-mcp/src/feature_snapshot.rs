use std::sync::Arc;

use serde_json::Value;
use tokio::time::Instant;

use crate::error::McpError;
use crate::feature_catalog::{FeatureCatalog, FeatureCatalogs};
use crate::features::prompts::{self, Prompt, validate_arguments_json};
use crate::features::resources::uri_template::{
    DEFAULT_TEMPLATE_MATCH_STEPS, TemplateMatch, TemplateMatchBudget, match_template_with_budget,
};
use crate::features::resources::{Resource, ResourceTemplate};
use crate::server_lifecycle::Server;

#[derive(Debug, Clone)]
pub(crate) enum FeatureIdentity {
    Resource(Arc<[Resource]>),
    Template(Arc<[ResourceTemplate]>),
    Prompt(Arc<[Prompt]>),
}

impl FeatureIdentity {
    pub(crate) fn current(&self, features: &FeatureCatalogs) -> bool {
        match self {
            Self::Resource(items) => published(features, items),
            Self::Template(items) => published(features, items),
            Self::Prompt(items) => published(features, items),
        }
    }
}

fn published<T: FeatureCatalog + PartialEq>(features: &FeatureCatalogs, items: &Arc<[T]>) -> bool {
    features
        .snapshot::<T>()
        .is_some_and(|snapshot| snapshot.items == *items)
}

impl Server {
    pub(crate) async fn resource_identity(
        self: &Arc<Self>,
        uri: &str,
        deadline: Instant,
    ) -> Result<FeatureIdentity, McpError> {
        let resources = self.feature_catalog::<Resource>(deadline).await?;
        if resources.iter().any(|resource| resource.uri == uri) {
            return Ok(FeatureIdentity::Resource(resources));
        }
        let templates = self.feature_catalog::<ResourceTemplate>(deadline).await?;
        let mut budget = TemplateMatchBudget::new(DEFAULT_TEMPLATE_MATCH_STEPS);
        for template in templates.iter() {
            check_deadline(deadline)?;
            match match_template_with_budget(&template.uri_template, uri, &mut budget) {
                TemplateMatch::Matches => return Ok(FeatureIdentity::Template(templates)),
                TemplateMatch::NoMatch => {}
                TemplateMatch::WorkLimitExceeded => {
                    return Err(McpError::McpResourceTemplateMatchLimitExceeded);
                }
            }
        }
        check_deadline(deadline)?;
        Err(McpError::McpResourceNotFound)
    }

    pub(crate) async fn prompt_identity(
        self: &Arc<Self>,
        name: &str,
        arguments_json: &str,
        deadline: Instant,
    ) -> Result<(FeatureIdentity, Value), McpError> {
        let catalog = self.feature_catalog::<Prompt>(deadline).await?;
        let prompt = catalog
            .iter()
            .find(|prompt| prompt.name == name)
            .ok_or(McpError::McpPromptNotFound)?;
        let arguments =
            validate_arguments_json(prompt, arguments_json, prompts::Limits::default())?;
        Ok((FeatureIdentity::Prompt(catalog), arguments))
    }
}

fn check_deadline(deadline: Instant) -> Result<(), McpError> {
    if Instant::now() >= deadline {
        return Err(McpError::McpRequestTimedOut);
    }
    Ok(())
}
