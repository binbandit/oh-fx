mod cursor;

use std::cmp::Ordering;

use crate::lexical_relevance::{PreparedQuery, contains_complete_identity};

const PAGE_LIMIT: usize = 5;
const PRIMARY_WEIGHT: f64 = 3.0;
const K1: f64 = 1.2;
const B: f64 = 0.75;
const MIN_SECONDARY_EVIDENCE_TOKEN_BYTES: usize = 4;
const RARE_SHORT_TOKEN_CATALOG_DIVISOR: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    Skill,
    Mcp,
}

#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub query: &'a PreparedQuery,
    pub server: Option<&'a str>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Document<'a> {
    pub identities: [&'a str; 2],
    pub stable_key: &'a str,
    pub primary: [&'a str; 4],
    pub primary_extra: &'a [String],
    pub secondary: [&'a str; 3],
}

impl Document<'_> {
    fn primary_fields(&self) -> impl Iterator<Item = &str> {
        self.primary
            .iter()
            .copied()
            .chain(self.primary_extra.iter().map(String::as_str))
    }
}

#[derive(Debug)]
pub struct Page {
    pub matches: Vec<usize>,
    pub total_matches: usize,
    fingerprint: u64,
    request_hash: u64,
    domain: Domain,
}

impl Page {
    #[must_use]
    pub fn cursor_after(&self, retained: usize) -> Option<String> {
        (retained < self.total_matches)
            .then(|| cursor::render(self.domain, self.fingerprint, self.request_hash, retained))
    }
}

struct Ranked {
    index: usize,
    exact: bool,
    score: f64,
    primary_hits: usize,
}

#[must_use]
pub fn retrieve(request: Request<'_>, domain: Domain, documents: &[Document<'_>]) -> Page {
    let query = request.query;
    let inventory = query.raw().is_empty();
    let primary_average = average(documents, primary_length);
    let secondary_average = average(documents, secondary_length);
    let frequencies: Vec<usize> = query
        .tokens()
        .map(|token| {
            documents
                .iter()
                .filter(|document| {
                    primary_frequency(document, token) > 0
                        || secondary_frequency(document, token) > 0
                })
                .count()
        })
        .collect();
    let mut ranked = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let exact = contains_complete_identity(query.raw(), &document.identities)
            || contains_complete_identity(query.raw(), &document.primary[..1]);
        let mut primary_hits = 0usize;
        let mut secondary_hits = 0usize;
        let mut score = 0.0;
        let primary_length = primary_length(document);
        let secondary_length = secondary_length(document);
        for (token, frequency) in query.tokens().zip(&frequencies) {
            let primary = primary_frequency(document, token);
            let secondary = secondary_frequency(document, token);
            primary_hits += usize::from(primary > 0);
            secondary_hits += usize::from(
                secondary > 0
                    && secondary_evidence(
                        token,
                        *frequency,
                        documents.len(),
                        request.server.is_some(),
                    ),
            );
            if primary == 0 && secondary == 0 {
                continue;
            }
            let idf = (1.0
                + (number(documents.len()) - number(*frequency) + 0.5)
                    / (number(*frequency) + 0.5))
                .ln();
            score += idf
                * (PRIMARY_WEIGHT * bm25(primary, primary_length, primary_average)
                    + bm25(secondary, secondary_length, secondary_average));
        }
        let clear = exact
            || (query.tokens().len() == 1 && primary_hits > 0)
            || primary_hits + secondary_hits >= 2;
        if inventory || clear {
            ranked.push(Ranked {
                index,
                exact,
                score,
                primary_hits,
            });
        }
    }
    ranked.sort_by(|left, right| {
        let relevance = if inventory {
            Ordering::Equal
        } else {
            right
                .exact
                .cmp(&left.exact)
                .then_with(|| right.score.total_cmp(&left.score))
                .then_with(|| right.primary_hits.cmp(&left.primary_hits))
        };
        relevance
            .then_with(|| {
                stable_order(
                    documents[left.index].stable_key,
                    documents[right.index].stable_key,
                )
            })
            .then_with(|| left.index.cmp(&right.index))
    });
    Page {
        total_matches: ranked.len(),
        matches: ranked
            .into_iter()
            .take(PAGE_LIMIT)
            .map(|ranked| ranked.index)
            .collect(),
        fingerprint: cursor::fingerprint(documents),
        request_hash: cursor::request_hash(request, domain),
        domain,
    }
}

fn secondary_evidence(
    token: &str,
    frequency: usize,
    documents: usize,
    server_scoped: bool,
) -> bool {
    if token.len() < MIN_SECONDARY_EVIDENCE_TOKEN_BYTES {
        return frequency <= documents / RARE_SHORT_TOKEN_CATALOG_DIVISOR;
    }
    server_scoped || frequency < documents
}

fn average(documents: &[Document<'_>], length: fn(&Document<'_>) -> usize) -> f64 {
    let total = documents
        .iter()
        .map(length)
        .fold(0usize, usize::saturating_add);
    (number(total) / number(documents.len().max(1))).max(1.0)
}

fn primary_length(document: &Document<'_>) -> usize {
    document
        .primary_fields()
        .map(token_count)
        .fold(0, usize::saturating_add)
}

fn secondary_length(document: &Document<'_>) -> usize {
    document
        .secondary
        .iter()
        .map(|field| token_count(field))
        .fold(0, usize::saturating_add)
}

fn primary_frequency(document: &Document<'_>, token: &str) -> usize {
    document
        .primary_fields()
        .map(|field| term_frequency(field, token))
        .fold(0, usize::saturating_add)
}

fn secondary_frequency(document: &Document<'_>, token: &str) -> usize {
    document
        .secondary
        .iter()
        .map(|field| term_frequency(field, token))
        .fold(0, usize::saturating_add)
}

fn token_count(text: &str) -> usize {
    text.as_bytes()
        .split(|byte| !byte.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .count()
}

fn stable_order(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
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
    frequency * (K1 + 1.0) / (frequency + K1 * (1.0 - B + B * number(length) / average))
}

fn number(value: usize) -> f64 {
    let bytes = u64::try_from(value).unwrap_or(u64::MAX).to_le_bytes();
    let low = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let high = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    f64::from(high) * 4_294_967_296.0 + f64::from(low)
}

#[cfg(test)]
mod tests;
