use ofx_config::ProviderId;

const CONFIGURED_ROUTE_NAME: &str = "Configured provider";

struct Entry {
    id: ProviderId,
    slug: &'static str,
    aliases: &'static [&'static str],
    route_name: &'static str,
}

static ENTRIES: [Entry; 3] = [
    Entry {
        id: ProviderId::Gateway,
        slug: "vercel",
        aliases: &["gateway", "ai-gateway"],
        route_name: "Vercel AI Gateway",
    },
    Entry {
        id: ProviderId::Codex,
        slug: "codex",
        aliases: &[],
        route_name: "Codex subscription",
    },
    Entry {
        id: ProviderId::Grok,
        slug: "grok",
        aliases: &[],
        route_name: "Grok subscription",
    },
];

pub fn parse(value: &str) -> Option<ProviderId> {
    ENTRIES
        .iter()
        .find(|entry| {
            std::iter::once(entry.slug)
                .chain(entry.aliases.iter().copied())
                .any(|name| value.eq_ignore_ascii_case(name))
        })
        .map(|entry| entry.id.clone())
}

pub fn label(id: &ProviderId) -> &'static str {
    ENTRIES
        .iter()
        .find(|entry| entry.id == *id)
        .map_or(CONFIGURED_ROUTE_NAME, |entry| entry.route_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_provider_catalog_uses_the_model_provider_identity_and_explicit_aliases() {
        for (value, id) in [
            ("vercel", ProviderId::Gateway),
            ("gateway", ProviderId::Gateway),
            ("AI-Gateway", ProviderId::Gateway),
            ("codex", ProviderId::Codex),
            ("GROK", ProviderId::Grok),
        ] {
            assert_eq!(parse(value), Some(id), "{value}");
        }
        for value in ["openai-codex", "chatgpt", "unknown", "portkey", ""] {
            assert_eq!(parse(value), None, "{value}");
        }
        assert_eq!(label(&ProviderId::Codex), "Codex subscription");
        assert_eq!(
            label(&ProviderId::Configured("portkey".to_owned())),
            "Configured provider"
        );
    }
}
