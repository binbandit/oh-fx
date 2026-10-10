use std::ops::Range;

const MAX_QUERY_BYTES: usize = 4 * 1024;

#[derive(Debug)]
pub struct PreparedQuery {
    raw: String,
    tokens: Vec<Range<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryTooLong;

impl PreparedQuery {
    pub fn prepare(raw: String) -> Result<Self, QueryTooLong> {
        if raw.len() > MAX_QUERY_BYTES {
            return Err(QueryTooLong);
        }
        let mut tokens: Vec<Range<usize>> = Vec::new();
        let mut start = None;
        for (offset, byte) in raw.bytes().chain(std::iter::once(0)).enumerate() {
            if byte.is_ascii_alphanumeric() {
                start.get_or_insert(offset);
            } else if let Some(begin) = start.take() {
                let token = &raw[begin..offset];
                if !tokens
                    .iter()
                    .any(|range| raw[range.clone()].eq_ignore_ascii_case(token))
                {
                    tokens.push(begin..offset);
                }
            }
        }
        Ok(Self { raw, tokens })
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub(crate) fn tokens(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tokens.iter().map(|range| &self.raw[range.clone()])
    }
}

pub(crate) fn contains_complete_identity(raw: &str, identities: &[&str]) -> bool {
    let raw = raw.as_bytes();
    identities.iter().any(|identity| {
        let identity = identity.as_bytes();
        !identity.is_empty()
            && raw
                .windows(identity.len())
                .enumerate()
                .any(|(start, value)| {
                    value.eq_ignore_ascii_case(identity)
                        && (start == 0 || !is_identity_byte(raw[start - 1]))
                        && (start + identity.len() == raw.len()
                            || !is_identity_byte(raw[start + identity.len()]))
                })
    })
}

fn is_identity_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_tokens_deduplicate_without_changing_raw_query() {
        let query = PreparedQuery::prepare("Foo foo FOO café 😀".to_owned()).unwrap();
        assert_eq!(query.raw(), "Foo foo FOO café 😀");
        assert_eq!(query.tokens().collect::<Vec<_>>(), ["Foo", "caf"]);
    }

    #[test]
    fn prepared_queries_accept_long_requests_within_the_byte_bound() {
        let query = "workflow ".repeat(100);
        let prepared = PreparedQuery::prepare(query.clone()).unwrap();
        assert_eq!(prepared.raw(), query);
        assert_eq!(prepared.tokens().collect::<Vec<_>>(), ["workflow"]);
    }

    #[test]
    fn prepared_queries_enforce_bounds_and_deduplicate_case_insensitively() {
        let prepared = PreparedQuery::prepare("GitHub github GITHUB issue".to_owned()).unwrap();
        assert_eq!(prepared.tokens().collect::<Vec<_>>(), ["GitHub", "issue"]);
        let maximum = PreparedQuery::prepare("a".repeat(MAX_QUERY_BYTES)).unwrap();
        assert_eq!(
            maximum.tokens().next(),
            Some("a".repeat(MAX_QUERY_BYTES).as_str())
        );
        assert_eq!(
            PreparedQuery::prepare("a".repeat(MAX_QUERY_BYTES + 1)).unwrap_err(),
            QueryTooLong
        );
    }

    #[test]
    fn identities_match_only_between_identity_boundaries() {
        assert!(contains_complete_identity(
            "use datadog now",
            &["", "DataDog"]
        ));
        assert!(contains_complete_identity("datadog", &["datadog"]));
        assert!(!contains_complete_identity("datadog-ops", &["datadog"]));
        assert!(!contains_complete_identity("my_datadog", &["datadog"]));
        assert!(contains_complete_identity("(datadog)", &["datadog"]));
        assert!(!contains_complete_identity("data", &["datadog"]));
        assert!(!contains_complete_identity("anything", &[""]));
    }
}
