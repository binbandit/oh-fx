use std::sync::Arc;

use ofx_config::ProviderDefinition;
use ofx_contract::{
    BoxFuture, CapabilityLookup, CapabilityResolver, CatalogRetry, ModelCapabilities, ModelCatalog,
    ModelCatalogSource, ModelOption,
};
use ofx_gateway::{CatalogFailure, CodexModel};
use tokio_util::sync::CancellationToken;

use crate::codex_provider::CatalogCapabilities;

#[derive(Clone)]
pub(crate) enum ModelSource {
    Connection(Arc<ProviderDefinition>),
    Codex(Arc<CatalogCapabilities>),
    Unavailable,
}

impl ModelSource {
    pub(crate) fn cached(&self) -> Option<ModelCatalog> {
        match self {
            Self::Connection(connection) => Some(connection_catalog(connection)),
            Self::Codex(catalog) => catalog.cached().map(codex_catalog),
            Self::Unavailable => Some(ModelCatalog::Failed { retry: None }),
        }
    }

    pub(crate) async fn catalog(&self) -> ModelCatalog {
        match self {
            Self::Connection(connection) => connection_catalog(connection),
            Self::Codex(catalog) => match catalog.listed(&CancellationToken::new()).await {
                Ok(listed) => codex_catalog(listed),
                Err(failure) => ModelCatalog::Failed {
                    retry: catalog_retry(failure),
                },
            },
            Self::Unavailable => ModelCatalog::Failed { retry: None },
        }
    }
}

fn connection_catalog(connection: &ProviderDefinition) -> ModelCatalog {
    ModelCatalog::Listed {
        models: connection
            .models()
            .iter()
            .map(|id| ModelOption {
                id: id.clone(),
                capabilities: connection_capabilities(connection, id),
                max_output_tokens: connection.capabilities(id).max_output_tokens,
            })
            .collect(),
        source: ModelCatalogSource::ProfileSettings,
    }
}

fn codex_catalog(listed: &[CodexModel]) -> ModelCatalog {
    ModelCatalog::Listed {
        models: listed
            .iter()
            .map(|model| ModelOption {
                id: model.id.clone(),
                capabilities: model.capabilities.clone(),
                max_output_tokens: None,
            })
            .collect(),
        source: ModelCatalogSource::Subscription,
    }
}

impl CapabilityResolver for ModelSource {
    fn resolve<'a>(
        &'a self,
        model: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup> {
        match self {
            Self::Connection(connection) => {
                let capabilities = connection_capabilities(connection, model);
                Box::pin(async move { CapabilityLookup::Resolved(capabilities) })
            }
            Self::Codex(catalog) => catalog.resolve(model, cancel),
            Self::Unavailable => Box::pin(async { CapabilityLookup::CatalogUnavailable }),
        }
    }
}

fn connection_capabilities(connection: &ProviderDefinition, model: &str) -> ModelCapabilities {
    ModelCapabilities {
        context_window: connection.capabilities(model).context_window,
        ..ModelCapabilities::default()
    }
}

fn catalog_retry(failure: CatalogFailure) -> Option<CatalogRetry> {
    match failure {
        CatalogFailure::RateLimited => Some(CatalogRetry::RateLimited),
        failure => failure.retryable().then_some(CatalogRetry::Unreachable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_retryable_catalog_failures_offer_a_retry() {
        assert_eq!(
            catalog_retry(CatalogFailure::RateLimited),
            Some(CatalogRetry::RateLimited)
        );
        for failure in [
            CatalogFailure::Transport,
            CatalogFailure::GatewayUnavailable { retryable: true },
        ] {
            assert_eq!(catalog_retry(failure), Some(CatalogRetry::Unreachable));
        }
        for failure in [
            CatalogFailure::GatewayUnavailable { retryable: false },
            CatalogFailure::Authentication,
            CatalogFailure::Cancellation,
            CatalogFailure::MalformedResponse,
            CatalogFailure::HttpStatus,
        ] {
            assert_eq!(catalog_retry(failure), None, "{failure:?}");
        }
    }
}
