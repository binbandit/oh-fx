use std::sync::Arc;

use ofx_config::ProviderDefinition;
use ofx_contract::{
    BoxFuture, CapabilityLookup, CapabilityResolver, CatalogRetry, ModelCapabilities, ModelCatalog,
    ModelCatalogSource, ModelControls, ModelOption, ReasoningEffort, intrinsically_fast,
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

    pub(crate) async fn ready(&self) {
        match self {
            Self::Connection(_) | Self::Unavailable => std::future::pending().await,
            Self::Codex(catalog) => catalog.ready().await,
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

pub(crate) fn model_controls(
    catalog: Option<&ModelCatalog>,
    model: &str,
    effort: &ReasoningEffort,
    fast: bool,
) -> ModelControls {
    let Some(catalog) = catalog else {
        return ModelControls {
            effort: effort.clone(),
            effort_supported: *effort != ReasoningEffort::Auto,
            fast: fast || intrinsically_fast(model),
        };
    };
    let efforts = match catalog {
        ModelCatalog::Listed { models, .. } => models
            .iter()
            .find(|option| option.id == model)
            .map_or(&[][..], |option| &option.capabilities.reasoning_efforts),
        ModelCatalog::Failed { .. } => &[],
    };
    let offered = match effort {
        ReasoningEffort::Auto => true,
        ReasoningEffort::Named(name) => efforts.contains(name),
    };
    ModelControls {
        effort: if offered {
            effort.clone()
        } else {
            ReasoningEffort::Auto
        },
        effort_supported: !efforts.is_empty(),
        fast: fast || intrinsically_fast(model),
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
    use ofx_contract::ReasoningEffort;

    use super::*;

    fn named(effort: &str) -> ReasoningEffort {
        ReasoningEffort::Named(effort.to_owned())
    }

    fn catalog_with(efforts: &[&str], fast: bool) -> ModelCatalog {
        ModelCatalog::Listed {
            models: vec![ModelOption {
                id: "openai/gpt-5".to_owned(),
                capabilities: ModelCapabilities {
                    reasoning_efforts: efforts.iter().map(|effort| (*effort).to_owned()).collect(),
                    supports_fast_mode: fast,
                    context_window: None,
                },
                max_output_tokens: None,
            }],
            source: ModelCatalogSource::Subscription,
        }
    }

    #[test]
    fn configured_controls_stay_visible_while_model_capabilities_load() {
        assert_eq!(
            model_controls(None, "anthropic/claude-opus-4.8", &named("xhigh"), true),
            ModelControls {
                effort: named("xhigh"),
                effort_supported: true,
                fast: true,
            }
        );
        assert_eq!(
            model_controls(None, "openai/gpt-5", &ReasoningEffort::Auto, false),
            ModelControls::default()
        );
    }

    #[test]
    fn listed_capabilities_decide_the_effort_and_keep_a_bound_fast_preference() {
        let low = catalog_with(&["low"], true);
        assert_eq!(
            model_controls(Some(&low), "openai/gpt-5", &named("low"), true),
            ModelControls {
                effort: named("low"),
                effort_supported: true,
                fast: true,
            }
        );
        assert_eq!(
            model_controls(Some(&low), "openai/gpt-5", &named("xhigh"), false),
            ModelControls {
                effort: ReasoningEffort::Auto,
                effort_supported: true,
                fast: false,
            }
        );
        let plain = catalog_with(&[], false);
        assert_eq!(
            model_controls(Some(&plain), "openai/gpt-5", &named("low"), true),
            ModelControls {
                effort: ReasoningEffort::Auto,
                effort_supported: false,
                fast: true,
            }
        );
    }

    #[test]
    fn a_model_named_fast_always_shows_the_fast_marker() {
        let plain = catalog_with(&[], false);
        for catalog in [None, Some(&plain)] {
            assert!(
                model_controls(catalog, "zai/glm-5.2-fast", &ReasoningEffort::Auto, false).fast
            );
            assert!(
                !model_controls(catalog, "provider/breakfast", &ReasoningEffort::Auto, false).fast
            );
        }
    }

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
