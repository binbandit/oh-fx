struct Entry {
    slug: &'static str,
    aliases: &'static [&'static str],
}

static ENTRIES: [Entry; 3] = [
    Entry {
        slug: "vercel",
        aliases: &["gateway", "ai-gateway"],
    },
    Entry {
        slug: "codex",
        aliases: &[],
    },
    Entry {
        slug: "grok",
        aliases: &[],
    },
];

pub fn is_login_provider(value: &str) -> bool {
    ENTRIES.iter().any(|entry| {
        std::iter::once(entry.slug)
            .chain(entry.aliases.iter().copied())
            .any(|name| value.eq_ignore_ascii_case(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_provider_catalog_uses_the_model_provider_identity_and_explicit_aliases() {
        for value in ["vercel", "gateway", "AI-Gateway", "codex", "GROK"] {
            assert!(is_login_provider(value), "{value}");
        }
        for value in ["openai-codex", "chatgpt", "unknown", "portkey", ""] {
            assert!(!is_login_provider(value), "{value}");
        }
    }
}
