use ofx_text::contains_ignore_case;

const MAX_QUERY_TOKENS: usize = 16;

pub fn resolve_model_query_from_ids<'a>(ids: &'a [String], query: &str) -> Option<&'a str> {
    if let Some(exact) = ids.iter().find(|id| id.eq_ignore_ascii_case(query)) {
        return Some(exact);
    }
    let mut best: Option<(i32, &str)> = None;
    for id in ids {
        let score = fuzzy_model_score(id, query);
        if score > best.map_or(0, |(current, _)| current) {
            best = Some((score, id));
        }
    }
    best.map(|(_, id)| id)
}

fn fuzzy_model_score(id: &str, query: &str) -> i32 {
    if query.is_empty() || id.is_empty() {
        return 0;
    }
    if contains_ignore_case(id, query) {
        let bonus = clamped(query.len(), 50);
        let suffix = id.len() > query.len()
            && id.as_bytes()[id.len() - query.len() - 1] == b'/'
            && id.as_bytes()[id.len() - query.len()..].eq_ignore_ascii_case(query.as_bytes());
        return if id.starts_with(query) || suffix {
            120 + bonus
        } else {
            100 + bonus
        };
    }
    let tokens = split_query_tokens(query);
    if tokens.len() > 1 && tokens.iter().all(|token| contains_ignore_case(id, token)) {
        return 80 + clamped(tokens.len(), MAX_QUERY_TOKENS) * 5;
    }
    let query_bytes = query.as_bytes();
    let mut matched = 0;
    for byte in id.bytes() {
        if matched < query_bytes.len() && byte.eq_ignore_ascii_case(&query_bytes[matched]) {
            matched += 1;
        }
    }
    if matched == query_bytes.len() {
        return 40 + clamped(matched, 20);
    }
    if tokens.iter().any(|token| contains_ignore_case(id, token)) {
        return 20;
    }
    0
}

fn split_query_tokens(query: &str) -> Vec<&str> {
    query
        .split(is_splitter)
        .filter(|token| !token.is_empty())
        .take(MAX_QUERY_TOKENS)
        .collect()
}

fn is_splitter(character: char) -> bool {
    matches!(character, ' ' | '-' | '/' | '_')
}

fn clamped(value: usize, limit: usize) -> i32 {
    i32::try_from(value.min(limit)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn session_commands_fuzzy_model_score_returns_high_score_for_exact_substring() {
        let score = fuzzy_model_score("anthropic/claude-sonnet-4-20250514", "claude-sonnet");
        assert!(score >= 100);
    }

    #[test]
    fn session_commands_fuzzy_model_score_returns_higher_score_for_prefix_match() {
        let prefix = fuzzy_model_score("openai/gpt-4o", "openai/gpt-4o");
        let substring = fuzzy_model_score("openai/gpt-4o-mini", "gpt-4o");
        assert!(prefix > substring);
    }

    #[test]
    fn session_commands_fuzzy_model_score_returns_positive_for_multi_token_match() {
        let score = fuzzy_model_score("anthropic/claude-sonnet-4-20250514", "claude sonnet");
        assert!(score >= 80);
    }

    #[test]
    fn session_commands_fuzzy_model_score_returns_positive_for_subsequence_match() {
        let score = fuzzy_model_score("anthropic/claude-sonnet-4-20250514", "anth son");
        assert!(score > 0);
    }

    #[test]
    fn session_commands_fuzzy_model_score_returns_zero_for_no_match() {
        assert_eq!(fuzzy_model_score("openai/gpt-4o", "zzzzz"), 0);
    }

    #[test]
    fn session_commands_fuzzy_model_score_returns_zero_for_empty_query() {
        assert_eq!(fuzzy_model_score("openai/gpt-4o", ""), 0);
    }

    #[test]
    fn session_commands_split_query_tokens_splits_on_separators() {
        assert_eq!(
            split_query_tokens("claude-sonnet/fast"),
            ["claude", "sonnet", "fast"]
        );
    }

    #[test]
    fn session_commands_is_splitter_recognizes_separator_characters() {
        for separator in [' ', '-', '/', '_'] {
            assert!(is_splitter(separator));
        }
        assert!(!is_splitter('a'));
        assert!(!is_splitter('0'));
    }

    #[test]
    fn fuzzy_scores_follow_the_upstream_tiers() {
        assert_eq!(fuzzy_model_score("openai/gpt-5", "openai"), 126);
        assert_eq!(fuzzy_model_score("openai/gpt-5", "gpt-5"), 125);
        assert_eq!(fuzzy_model_score("openai/gpt-5", "pt-5"), 104);
        assert_eq!(fuzzy_model_score("claude-sonnet-4.5", "sonnet claude"), 90);
        assert_eq!(fuzzy_model_score("claude-sonnet-4.5", "cs45"), 44);
        assert_eq!(fuzzy_model_score("claude-sonnet-4.5", "zzz sonnet"), 20);
    }

    #[test]
    fn exact_matches_win_regardless_of_case() {
        let models = ids(&["openai/gpt-5", "OpenAI/GPT-5-mini"]);
        assert_eq!(
            resolve_model_query_from_ids(&models, "openai/gpt-5-MINI"),
            Some("OpenAI/GPT-5-mini")
        );
    }

    #[test]
    fn fuzzy_queries_pick_the_best_scoring_model() {
        let models = ids(&[
            "@openai/gpt-4o",
            "anthropic/claude-sonnet-4.5",
            "fake-model",
        ]);
        let resolve = |query| resolve_model_query_from_ids(&models, query);
        assert_eq!(resolve("sonnet"), Some("anthropic/claude-sonnet-4.5"));
        assert_eq!(resolve("gpt-4o"), Some("@openai/gpt-4o"));
        assert_eq!(resolve("fkmdl"), Some("fake-model"));
        assert_eq!(resolve("unknown-id"), None);
        assert_eq!(resolve_model_query_from_ids(&[], "anything"), None);
    }
}
