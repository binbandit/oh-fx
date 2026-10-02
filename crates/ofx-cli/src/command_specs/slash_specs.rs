use super::matches_command_token;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlashKind {
    Help,
    ClearScreen,
    Model,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SlashArguments {
    None,
    Payload,
}

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashSpec {
    pub(crate) kind: SlashKind,
    pub command: &'static str,
    pub aliases: &'static [&'static str],
    pub completion_description: &'static str,
    pub(crate) arguments: SlashArguments,
}

impl SlashSpec {
    pub(crate) const fn new(
        kind: SlashKind,
        command: &'static str,
        completion_description: &'static str,
    ) -> Self {
        Self {
            kind,
            command,
            aliases: &[],
            completion_description,
            arguments: SlashArguments::None,
        }
    }

    pub(crate) const fn with_aliases(self, aliases: &'static [&'static str]) -> Self {
        Self { aliases, ..self }
    }

    pub(crate) const fn with_payload(self) -> Self {
        Self {
            arguments: SlashArguments::Payload,
            ..self
        }
    }

    pub(crate) fn accepts_payload(&self) -> bool {
        self.arguments == SlashArguments::Payload
    }

    pub(crate) fn tokens(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.command).chain(self.aliases.iter().copied())
    }

    pub(crate) fn exact_token(&self, input: &str) -> Option<&'static str> {
        self.tokens()
            .find(|token| matches_command_token(input, token))
    }

    pub(crate) fn prefix_token(&self, input: &str) -> Option<&'static str> {
        self.tokens().find(|token| {
            input
                .strip_prefix(token)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::SLASH_REGISTRY;

    fn spec(kind: SlashKind) -> &'static SlashSpec {
        SLASH_REGISTRY
            .commands()
            .iter()
            .find(|spec| spec.kind == kind)
            .unwrap()
    }

    #[test]
    fn slash_prefix_matcher_accepts_tab_boundary_and_rejects_newline_boundary() {
        let model = spec(SlashKind::Model);
        assert_eq!(model.prefix_token("/model\tmodel-id"), Some("/model"));
        assert_eq!(model.prefix_token("/model\nmodel-id"), None);
        assert_eq!(model.prefix_token("/modelx"), None);
    }

    #[test]
    fn prefix_matching_requires_a_space_or_tab_boundary_after_the_token() {
        let model = spec(SlashKind::Model);
        assert_eq!(model.prefix_token("/model gpt-5"), Some("/model"));
        assert_eq!(model.prefix_token("/model\tgpt-5"), Some("/model"));
        assert_eq!(model.prefix_token("/models"), None);
        assert_eq!(model.prefix_token("/mode"), None);
    }

    #[test]
    fn default_slash_registry_resolves_primary_commands_and_aliases() {
        let quit = spec(SlashKind::Quit);
        assert_eq!(quit.exact_token("/exit\t"), Some("/exit"));
        assert_eq!(quit.exact_token("/quit "), Some("/quit"));
        assert_eq!(quit.exact_token("/quit now"), None);
    }

    #[test]
    fn interactive_model_command_has_no_plural_spelling() {
        assert!(
            SLASH_REGISTRY
                .commands()
                .iter()
                .all(|spec| spec.exact_token("/models").is_none())
        );
    }

    #[test]
    fn slash_specs_keep_unique_commands_and_aliases() {
        let tokens = || SLASH_REGISTRY.commands().iter().flat_map(SlashSpec::tokens);
        for token in tokens() {
            assert_eq!(tokens().filter(|candidate| *candidate == token).count(), 1);
        }
    }

    #[test]
    fn the_registry_keeps_upstream_order_and_aliases() {
        let commands: Vec<&str> = SLASH_REGISTRY
            .commands()
            .iter()
            .map(|spec| spec.command)
            .collect();
        assert_eq!(commands, ["/help", "/clear", "/model", "/quit"]);
        assert_eq!(spec(SlashKind::Quit).aliases, ["/exit"]);
        assert_eq!(
            spec(SlashKind::ClearScreen).completion_description,
            "start a fresh conversation while keeping managed processes"
        );
    }
}
