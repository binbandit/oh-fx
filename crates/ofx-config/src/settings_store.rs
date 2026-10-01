pub(crate) const MAX_PROVIDER_ORDER_ENTRIES: usize = 8;
const MAX_PROVIDER_SLUG_BYTES: usize = 64;

pub(crate) fn validate_provider_slug(slug: &str) -> bool {
    slug.len() <= MAX_PROVIDER_SLUG_BYTES
        && slug.starts_with(|first: char| first.is_ascii_alphanumeric())
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_slugs_are_bounded_alphanumeric_names_with_dashes() {
        for slug in ["azure", "vertexAnthropic", "bedrock-2", "9router"] {
            assert!(validate_provider_slug(slug), "{slug}");
        }
        assert!(validate_provider_slug(&"a".repeat(MAX_PROVIDER_SLUG_BYTES)));
        for slug in ["", "-azure", "Bad Slug", "a_b", "a.b"] {
            assert!(!validate_provider_slug(slug), "{slug:?}");
        }
        assert!(!validate_provider_slug(
            &"a".repeat(MAX_PROVIDER_SLUG_BYTES + 1)
        ));
    }
}
