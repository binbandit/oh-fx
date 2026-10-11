use std::cmp::Ordering;

use super::GatewayModel;

const PICKER_PROVIDER_LIMIT: usize = 2;
const EXTENDED_PICKER_PROVIDER_LIMIT: usize = 3;
const FEATURED_FAMILIES: [&str; 5] = [
    "openai/gpt",
    "xai/grok-build",
    "zai/glm",
    "deepseek/deepseek",
    "minimax/minimax",
];
const RANKED_PROVIDERS: [&str; 7] = [
    "openai/",
    "google/",
    "xai/",
    "deepseek/",
    "meta/",
    "mistral/",
    "alibaba/",
];
const EXCLUDED_VARIANTS: [&str; 4] = ["-mini", "-nano", "-oss", "-safeguard"];
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

pub(super) fn picker(candidates: Vec<GatewayModel>) -> Vec<GatewayModel> {
    let mut chosen: Vec<usize> = Vec::with_capacity(candidates.len());
    for family in FEATURED_FAMILIES {
        if let Some(index) = newest(
            &candidates,
            &chosen,
            |candidate| family_of(&candidate.id) == family,
            |left, right| featured_is_newer(family, left, right),
        ) {
            chosen.push(index);
        }
    }
    let mut providers: Vec<&str> = Vec::new();
    for candidate in candidates
        .iter()
        .filter(|candidate| is_picker_candidate(candidate))
    {
        let provider = provider_of(&candidate.id);
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    providers.sort_unstable();
    for provider in providers {
        while chosen
            .iter()
            .filter(|index| provider_of(&candidates[**index].id) == provider)
            .count()
            < provider_limit(provider)
        {
            let Some(index) = newest(
                &candidates,
                &chosen,
                |candidate| {
                    provider_of(&candidate.id) == provider
                        && is_provider_candidate(provider, candidate, &candidates)
                },
                is_newer,
            ) else {
                break;
            };
            chosen.push(index);
        }
    }
    for (index, candidate) in candidates.iter().enumerate() {
        if !selected(&candidates, &chosen, &candidate.id) {
            chosen.push(index);
        }
    }
    let mut slots: Vec<Option<GatewayModel>> = candidates.into_iter().map(Some).collect();
    chosen
        .into_iter()
        .filter_map(|index| slots[index].take())
        .collect()
}

fn newest(
    candidates: &[GatewayModel],
    chosen: &[usize],
    eligible: impl Fn(&GatewayModel) -> bool,
    newer: impl Fn(&GatewayModel, &GatewayModel) -> bool,
) -> Option<usize> {
    let mut newest: Option<usize> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if !is_picker_candidate(candidate)
            || !eligible(candidate)
            || selected(candidates, chosen, &candidate.id)
        {
            continue;
        }
        if newest.is_none_or(|current| newer(candidate, &candidates[current])) {
            newest = Some(index);
        }
    }
    newest
}

fn selected(candidates: &[GatewayModel], chosen: &[usize], id: &str) -> bool {
    chosen.iter().any(|index| candidates[*index].id == id)
}

fn is_picker_candidate(candidate: &GatewayModel) -> bool {
    let id = &candidate.id;
    let glm_fast = id.starts_with("zai/glm-") && id.ends_with("-fast");
    !glm_fast
        && !EXCLUDED_VARIANTS
            .iter()
            .any(|variant| contains_ignore_case(id, variant))
}

fn family_of(id: &str) -> &str {
    if id.starts_with("openai/gpt-") && contains_ignore_case(id, "-codex") {
        return "openai/gpt-codex";
    }
    if id.starts_with("deepseek/deepseek-") {
        return "deepseek/deepseek";
    }
    if id.starts_with("minimax/minimax-") {
        return "minimax/minimax";
    }
    let Some(slash) = id.find('/') else {
        return id;
    };
    let bytes = id.as_bytes();
    (slash + 1..bytes.len().saturating_sub(1))
        .find(|index| bytes[*index] == b'-' && bytes[index + 1].is_ascii_digit())
        .map_or(id, |index| &id[..index])
}

fn provider_of(id: &str) -> &str {
    id.split_once('/').map_or(id, |(provider, _)| provider)
}

fn provider_limit(provider: &str) -> usize {
    if provider == "openai" {
        EXTENDED_PICKER_PROVIDER_LIMIT
    } else {
        PICKER_PROVIDER_LIMIT
    }
}

fn is_provider_candidate(
    provider: &str,
    candidate: &GatewayModel,
    candidates: &[GatewayModel],
) -> bool {
    if provider != "openai" {
        return true;
    }
    matches!(family_of(&candidate.id), "openai/gpt" | "openai/gpt-codex")
        && within_family_limit(candidate, candidates)
}

fn within_family_limit(candidate: &GatewayModel, candidates: &[GatewayModel]) -> bool {
    let family = family_of(&candidate.id);
    let newer = candidates
        .iter()
        .filter(|other| {
            is_picker_candidate(other)
                && family_of(&other.id) == family
                && is_newer(other, candidate)
        })
        .count();
    newer < family_limit(family)
}

fn family_limit(family: &str) -> usize {
    match family {
        "openai/gpt" => 2,
        "openai/gpt-codex" => 1,
        _ => EXTENDED_PICKER_PROVIDER_LIMIT,
    }
}

fn is_newer(left: &GatewayModel, right: &GatewayModel) -> bool {
    left.released
        .cmp(&right.released)
        .then_with(|| right.id.len().cmp(&left.id.len()))
        .then_with(|| right.id.cmp(&left.id))
        == Ordering::Greater
}

fn featured_is_newer(family: &str, left: &GatewayModel, right: &GatewayModel) -> bool {
    if left.released != right.released {
        return left.released > right.released;
    }
    if family == "deepseek/deepseek" {
        let (left_rank, right_rank) = (deepseek_rank(&left.id), deepseek_rank(&right.id));
        if left_rank != right_rank {
            return left_rank < right_rank;
        }
    }
    is_newer(left, right)
}

fn deepseek_rank(id: &str) -> u8 {
    if id.ends_with("-pro") {
        0
    } else if contains_ignore_case(id, "-thinking") || contains_ignore_case(id, "-reasoning") {
        1
    } else if contains_ignore_case(id, "-flash") {
        3
    } else {
        2
    }
}

#[cfg(test)]
mod tests;
