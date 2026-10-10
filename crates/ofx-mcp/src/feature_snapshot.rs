use std::sync::Arc;

use tokio::time::Instant;

use crate::error::McpError;
use crate::feature_catalog::{FeatureCatalog, FeatureCatalogs};
use crate::features::resources::uri_template::{
    DEFAULT_TEMPLATE_MATCH_STEPS, TemplateMatch, TemplateMatchBudget, match_template_with_budget,
};
use crate::features::resources::{Resource, ResourceTemplate};
use crate::server_lifecycle::Server;

#[derive(Debug, Clone)]
pub(crate) enum ResourceIdentity {
    Resource(Arc<[Resource]>),
    Template(Arc<[ResourceTemplate]>),
}

impl ResourceIdentity {
    pub(crate) fn current(&self, features: &FeatureCatalogs) -> bool {
        match self {
            Self::Resource(items) => published(features, items),
            Self::Template(items) => published(features, items),
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
    ) -> Result<ResourceIdentity, McpError> {
        let resources = self.feature_catalog::<Resource>(deadline).await?;
        if resources.iter().any(|resource| resource.uri == uri) {
            return Ok(ResourceIdentity::Resource(resources));
        }
        let templates = self.feature_catalog::<ResourceTemplate>(deadline).await?;
        let mut budget = TemplateMatchBudget::new(DEFAULT_TEMPLATE_MATCH_STEPS);
        for template in templates.iter() {
            check_deadline(deadline)?;
            match match_template_with_budget(&template.uri_template, uri, &mut budget) {
                TemplateMatch::Matches => return Ok(ResourceIdentity::Template(templates)),
                TemplateMatch::NoMatch => {}
                TemplateMatch::WorkLimitExceeded => {
                    return Err(McpError::McpResourceTemplateMatchLimitExceeded);
                }
            }
        }
        check_deadline(deadline)?;
        Err(McpError::McpResourceNotFound)
    }
}

fn check_deadline(deadline: Instant) -> Result<(), McpError> {
    if Instant::now() >= deadline {
        return Err(McpError::McpRequestTimedOut);
    }
    Ok(())
}
