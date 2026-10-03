use std::cmp::Ordering;
use std::ops::Range;

#[derive(Debug)]
pub struct PreparedQuery {
    raw: String,
    tokens: Vec<Range<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryTooLong;

impl PreparedQuery {
    pub fn prepare(raw: String) -> Result<Self, QueryTooLong> {
        if raw.len() > 4096 {
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

    fn tokens(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tokens.iter().map(|range| &self.raw[range.clone()])
    }
}

pub struct LexicalDocument<'a> {
    pub identity: &'a str,
    pub primary: &'a str,
    pub secondary: &'a str,
    pub stable_key: &'a str,
}

pub fn rank_intent(query: &PreparedQuery, documents: &[LexicalDocument<'_>]) -> Vec<usize> {
    let length = |text: &str| {
        text.as_bytes()
            .split(|b| !b.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .count()
    };
    let average = |primary: bool| {
        let total = documents
            .iter()
            .map(|d| length(if primary { d.primary } else { d.secondary }))
            .fold(0usize, usize::saturating_add);
        (number(total) / number(documents.len().max(1))).max(1.0)
    };
    let primary_average = average(true);
    let secondary_average = average(false);
    let frequencies: Vec<_> = query
        .tokens()
        .map(|token| {
            documents
                .iter()
                .filter(|d| {
                    term_frequency(d.primary, token) > 0 || term_frequency(d.secondary, token) > 0
                })
                .count()
        })
        .collect();
    let mut ranked = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let exact = contains_identity(query.raw(), document.identity)
            || contains_identity(query.raw(), document.primary);
        let mut primary_hits = 0usize;
        let mut secondary_hits = 0usize;
        let mut score = 0.0;
        for (token, frequency) in query.tokens().zip(&frequencies) {
            let primary = term_frequency(document.primary, token);
            let secondary = term_frequency(document.secondary, token);
            primary_hits += usize::from(primary > 0);
            let evidence = if token.len() < 4 {
                *frequency <= documents.len() / 32
            } else {
                *frequency < documents.len()
            };
            secondary_hits += usize::from(secondary > 0 && evidence);
            if primary > 0 || secondary > 0 {
                let idf = (1.0
                    + (number(documents.len()) - number(*frequency) + 0.5)
                        / (number(*frequency) + 0.5))
                    .ln();
                score += idf
                    * (3.0 * bm25(primary, length(document.primary), primary_average)
                        + bm25(secondary, length(document.secondary), secondary_average));
            }
        }
        if query.raw().is_empty()
            || exact
            || (query.tokens.len() == 1 && primary_hits > 0)
            || primary_hits + secondary_hits >= 2
        {
            ranked.push((index, exact, score, primary_hits));
        }
    }
    ranked.sort_by(|a, b| {
        let relevance = if query.raw().is_empty() {
            Ordering::Equal
        } else {
            b.1.cmp(&a.1)
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| b.3.cmp(&a.3))
        };
        relevance
            .then_with(|| stable_order(documents[a.0].stable_key, documents[b.0].stable_key))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.into_iter().map(|entry| entry.0).collect()
}

fn stable_order(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|b| b.to_ascii_lowercase())
        .cmp(right.bytes().map(|b| b.to_ascii_lowercase()))
}

fn contains_identity(text: &str, identity: &str) -> bool {
    let text = text.as_bytes();
    let identity = identity.as_bytes();
    let boundary = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
    !identity.is_empty()
        && text
            .windows(identity.len())
            .enumerate()
            .any(|(start, value)| {
                value.eq_ignore_ascii_case(identity)
                    && (start == 0 || !boundary(text[start - 1]))
                    && (start + identity.len() == text.len()
                        || !boundary(text[start + identity.len()]))
            })
}

fn term_frequency(text: &str, token: &str) -> usize {
    let text = text.as_bytes();
    let token = token.as_bytes();
    if token.is_empty() || token.len() > text.len() {
        return 0;
    }
    let mut count = 0;
    let mut start = 0;
    while start <= text.len() - token.len() {
        let end = start + token.len();
        if text[start..end].eq_ignore_ascii_case(token)
            && (start == 0 || !text[start - 1].is_ascii_alphanumeric())
            && (end == text.len() || !text[end].is_ascii_alphanumeric())
        {
            count += 1;
            start = end;
        } else {
            start += 1;
        }
    }
    count
}

fn bm25(frequency: usize, length: usize, average: f64) -> f64 {
    if frequency == 0 {
        return 0.0;
    }
    let frequency = number(frequency);
    frequency * 2.2 / (frequency + 1.2 * (0.25 + 0.75 * number(length) / average))
}

fn number(value: usize) -> f64 {
    let bytes = u64::try_from(value).unwrap_or(u64::MAX).to_le_bytes();
    let low = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let high = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    f64::from(high) * 4_294_967_296.0 + f64::from(low)
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
    fn stable_path_order_breaks_identical_rank_ties() {
        let documents = [
            LexicalDocument {
                identity: "review",
                primary: "review",
                secondary: "test",
                stable_key: "/B",
            },
            LexicalDocument {
                identity: "review",
                primary: "review",
                secondary: "test",
                stable_key: "/a",
            },
        ];
        assert_eq!(
            rank_intent(
                &PreparedQuery::prepare("review".to_owned()).unwrap(),
                &documents
            ),
            [1, 0]
        );
        assert_eq!(
            rank_intent(
                &PreparedQuery::prepare("Review review".to_owned()).unwrap(),
                &documents
            ),
            [1, 0]
        );
    }
}

#[cfg(test)]
mod long_intent_query {
    use super::*;
    #[test]
    fn more_than_255_distinct_evidence_tokens_do_not_overflow() {
        let raw = (0..256)
            .map(|i| format!("term{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(raw.len() <= 4096);
        let query = PreparedQuery::prepare(raw.clone()).unwrap();
        assert_eq!(query.tokens().len(), 256);
        let documents = [
            LexicalDocument {
                identity: "selected",
                primary: "selected",
                secondary: &raw,
                stable_key: "/a",
            },
            LexicalDocument {
                identity: "other",
                primary: "other",
                secondary: "",
                stable_key: "/b",
            },
        ];
        assert_eq!(rank_intent(&query, &documents), [0]);
    }
}
