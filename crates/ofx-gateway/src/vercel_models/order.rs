use std::cmp::Ordering;

use super::GatewayModel;

const RANKED_PROVIDERS: [&str; 7] = [
    "openai/",
    "google/",
    "xai/",
    "deepseek/",
    "meta/",
    "mistral/",
    "alibaba/",
];
const TOP_TIER_NAMES: [&str; 8] = ["opus", "sonnet", "gpt-5", "o1", "o3", "o4", "pro", "grok-4"];

pub(super) fn catalog_order(left: &GatewayModel, right: &GatewayModel) -> Ordering {
    right
        .tool_use
        .cmp(&left.tool_use)
        .then_with(|| tier_rank(&left.id).cmp(&tier_rank(&right.id)))
        .then_with(|| provider_rank(&left.id).cmp(&provider_rank(&right.id)))
        .then_with(|| right.released.cmp(&left.released))
        .then_with(|| left.id.cmp(&right.id))
}

fn provider_rank(id: &str) -> usize {
    RANKED_PROVIDERS
        .iter()
        .position(|prefix| id.starts_with(prefix))
        .map_or(RANKED_PROVIDERS.len() + 1, |position| position + 1)
}

fn tier_rank(id: &str) -> u8 {
    let has = |needle: &str| contains_ignore_case(id, needle);
    if has("preview") || has("beta") {
        4
    } else if has("haiku") || has("mini") || has("lite") {
        3
    } else if has("flash") {
        2
    } else {
        u8::from(!TOP_TIER_NAMES.iter().any(|needle| has(needle)))
    }
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests;
