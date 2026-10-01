const FULL_WINDOW_OUTPUT_TOKENS: u32 = 32_768;
const FULL_WINDOW_OUTPUT_DIVISOR: u32 = 8;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

pub fn request_output_tokens(capabilities: Capabilities) -> Option<u32> {
    let advertised = capabilities.max_output_tokens?;
    let Some(window) = capabilities.context_window else {
        return Some(advertised);
    };
    if advertised < window {
        return Some(advertised);
    }
    Some(FULL_WINDOW_OUTPUT_TOKENS.min(window / FULL_WINDOW_OUTPUT_DIVISOR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_output_limit_bounds_full_window_catalog_limits() {
        let cases = [
            ((None, None), None),
            ((None, Some(32_000)), Some(32_000)),
            ((Some(256_000), None), None),
            ((Some(256_000), Some(32_000)), Some(32_000)),
            ((Some(1_000_000), Some(1_000_000)), Some(32_768)),
            ((Some(131_072), Some(131_072)), Some(16_384)),
            ((Some(128_000), Some(256_000)), Some(16_000)),
        ];
        for ((context_window, max_output_tokens), expected) in cases {
            let capabilities = Capabilities {
                context_window,
                max_output_tokens,
            };
            assert_eq!(request_output_tokens(capabilities), expected);
        }
    }
}
