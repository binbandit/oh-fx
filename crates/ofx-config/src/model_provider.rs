use crate::configured_provider::validate_id;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderId {
    Gateway,
    Codex,
    Grok,
    Configured(String),
}

impl ProviderId {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        for (label, provider) in [
            ("gateway", Self::Gateway),
            ("codex", Self::Codex),
            ("grok", Self::Grok),
        ] {
            if text.eq_ignore_ascii_case(label) {
                return Some(provider);
            }
        }
        validate_id(text).ok()?;
        Some(Self::Configured(text.to_owned()))
    }

    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Gateway => "gateway",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Configured(id) => id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_parse_builtins_case_insensitively_and_validate_configured_ids() {
        assert_eq!(ProviderId::parse("Gateway"), Some(ProviderId::Gateway));
        assert_eq!(
            ProviderId::parse("CODEX").map(|id| id.label().to_owned()),
            Some("codex".to_owned())
        );
        assert_eq!(
            ProviderId::parse("portkey"),
            Some(ProviderId::Configured("portkey".to_owned()))
        );
        assert_eq!(ProviderId::parse("bad name"), None);
        assert_eq!(ProviderId::parse(""), None);
    }
}
