use tokio_util::sync::CancellationToken;

use crate::stream_provider::{BoxFuture, ProviderOptions};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImageInputSupport {
    #[default]
    Unknown,
    Native,
    NonNative,
}

impl ImageInputSupport {
    pub fn from_vision(supports_vision: bool) -> Self {
        if supports_vision {
            Self::Native
        } else {
            Self::NonNative
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub reasoning_efforts: Vec<String>,
    pub supports_fast_mode: bool,
    pub context_window: Option<u32>,
    pub image_input_support: ImageInputSupport,
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

pub fn intrinsically_fast(model: &str) -> bool {
    model.ends_with("-fast")
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
            image_input_support: ImageInputSupport::Unknown,
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
    fn capabilities_infer_intrinsic_fast_identity_but_not_controls_from_model_ids() {
        for (id, intrinsic_fast) in [
            ("openai/gpt-5.6-sol", false),
            ("anthropic/claude-opus-4.8", false),
            ("zai/glm-5.2", false),
            ("zai/glm-5.2-fast", true),
            ("provider/breakfast", false),
        ] {
            assert_eq!(intrinsically_fast(id), intrinsic_fast, "{id}");
        }
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
