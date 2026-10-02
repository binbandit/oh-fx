use tokio_util::sync::CancellationToken;

use crate::stream_provider::{BoxFuture, ProviderOptions};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub reasoning_efforts: Vec<String>,
    pub supports_fast_mode: bool,
    pub context_window: Option<u32>,
}

impl ModelCapabilities {
    pub fn provider_options<'a>(
        &self,
        reasoning_effort: Option<&'a str>,
        fast: bool,
    ) -> ProviderOptions<'a> {
        ProviderOptions {
            reasoning_effort: reasoning_effort.filter(|effort| {
                self.reasoning_efforts
                    .iter()
                    .any(|supported| supported == effort)
            }),
            fast: fast && self.supports_fast_mode,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityLookup {
    Resolved(ModelCapabilities),
    CatalogUnavailable,
    Cancelled,
}

pub trait CapabilityResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        model: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(efforts: &[&str], fast: bool) -> ModelCapabilities {
        ModelCapabilities {
            reasoning_efforts: efforts.iter().map(|effort| (*effort).to_owned()).collect(),
            supports_fast_mode: fast,
            context_window: None,
        }
    }

    #[test]
    fn provider_options_keep_only_what_the_model_supports() {
        let supported = capabilities(&["low", "high"], true);
        assert_eq!(
            supported.provider_options(Some("high"), true),
            ProviderOptions {
                reasoning_effort: Some("high"),
                fast: true,
            }
        );
        assert_eq!(
            supported.provider_options(Some("xhigh"), false),
            ProviderOptions::default()
        );
        let plain = capabilities(&[], false);
        assert_eq!(
            plain.provider_options(Some("low"), true),
            ProviderOptions::default()
        );
    }

    #[test]
    fn reasoning_efforts_match_by_exact_name() {
        let supported = capabilities(&["high"], false);
        assert_eq!(
            supported.provider_options(Some("HIGH"), false),
            ProviderOptions::default()
        );
    }
}
