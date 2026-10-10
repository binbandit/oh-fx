use std::sync::Arc;

use tokio::time::Instant;

use crate::error::McpError;
use crate::feature_catalog::{FeatureCatalog, FeatureCatalogs};
use crate::features::prompts::Prompt;
use crate::features::resources::uri_template::{
    DEFAULT_TEMPLATE_MATCH_STEPS, TemplateMatch, TemplateMatchBudget, match_template_with_budget,
};
use crate::features::resources::{Resource, ResourceTemplate};
use crate::server_connection::McpClient;
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

    fn invalidated(&self, client: &McpClient) -> bool {
        match self {
            Self::Resource(_) => Resource::invalidation(client).pending(),
            Self::Template(_) => ResourceTemplate::invalidation(client).pending(),
            Self::Prompt(_) => Prompt::invalidation(client).pending(),
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

    pub(crate) async fn prompt_identity<R>(
        self: &Arc<Self>,
        name: &str,
        deadline: Instant,
        check: impl FnOnce(&Prompt) -> Result<R, McpError>,
    ) -> Result<(FeatureIdentity, R), McpError> {
        let catalog = self.feature_catalog::<Prompt>(deadline).await?;
        let prompt = catalog
            .iter()
            .find(|prompt| prompt.name == name)
            .ok_or(McpError::McpPromptNotFound)?;
        let checked = check(prompt)?;
        Ok((FeatureIdentity::Prompt(catalog), checked))
    }

    pub(crate) async fn template_identity(
        self: &Arc<Self>,
        uri_template: &str,
        deadline: Instant,
    ) -> Result<FeatureIdentity, McpError> {
        let templates = self.feature_catalog::<ResourceTemplate>(deadline).await?;
        if templates
            .iter()
            .any(|template| template.uri_template == uri_template)
        {
            return Ok(FeatureIdentity::Template(templates));
        }
        Err(McpError::McpResourceTemplateNotFound)
    }

    pub(crate) fn check_current(
        &self,
        client: &Arc<McpClient>,
        identity: &FeatureIdentity,
    ) -> Result<(), McpError> {
        let current = self
            .while_current(client, || identity.current(&self.features))
            .unwrap_or(false);
        if !current || identity.invalidated(client) {
            return Err(McpError::McpFeatureCatalogChanged);
        }
        Ok(())
    }
}

fn check_deadline(deadline: Instant) -> Result<(), McpError> {
    if Instant::now() >= deadline {
        return Err(McpError::McpRequestTimedOut);
    }
    Ok(())
}
