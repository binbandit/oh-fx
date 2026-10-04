use super::matches_command_token;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlashKind {
    Help,
    ClearScreen,
    NewSession,
    ResetSession,
    ResumeSession,
    RenameSession,
    Login,
    Logout,
    Provider,
    Stats,
    Usage,
    Status,
    Model,
    Permissions,
    Allowlist,
    Undo,
    Mcp,
    Skills,
    Copy,
    Feedback,
    Compact,
    Settings,
    Alias,
    Fast,
    Ultrafast,
    Statusline,
    Workspace,
    Shell,
    Version,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlashPresentationCategory {
    General,
    Session,
    Account,
    Model,
    Appearance,
    Security,
    Workspace,
    Media,
    Agents,
    Extensions,
    Product,
}

impl SlashPresentationCategory {
    pub const ALL: [Self; 11] = [
        Self::General,
        Self::Session,
        Self::Account,
        Self::Model,
        Self::Appearance,
        Self::Security,
        Self::Workspace,
        Self::Media,
        Self::Agents,
        Self::Extensions,
        Self::Product,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Session => "Session",
            Self::Account => "Account",
            Self::Model => "Model",
            Self::Appearance => "Appearance",
            Self::Security => "Security",
            Self::Workspace => "Workspace",
            Self::Media => "Media",
            Self::Agents => "Agents",
            Self::Extensions => "Extensions",
            Self::Product => "Product",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SlashArguments {
    None,
    Payload,
}

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashSpec {
    pub kind: SlashKind,
    pub command: &'static str,
    pub aliases: &'static [&'static str],
    pub completion_description: &'static str,
    pub help_entry: &'static str,
    pub presentation_category: SlashPresentationCategory,
    pub(crate) arguments: SlashArguments,
}

impl SlashSpec {
    pub(crate) const fn new(
        kind: SlashKind,
        command: &'static str,
        completion_description: &'static str,
        presentation_category: SlashPresentationCategory,
    ) -> Self {
        Self {
            kind,
            command,
            aliases: &[],
            completion_description,
            help_entry: command,
            presentation_category,
            arguments: SlashArguments::None,
        }
    }

    pub(crate) const fn with_help(self, help_entry: &'static str) -> Self {
        Self { help_entry, ..self }
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

    pub fn accepts_payload(&self) -> bool {
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
    fn login_and_provider_choose_the_provider_without_an_argument() {
        assert_eq!(spec(SlashKind::Provider).aliases, ["/setup"]);
        for kind in [SlashKind::Login, SlashKind::Provider] {
            assert_eq!(
                spec(kind).completion_description,
                "choose the model provider and how it signs in"
            );
            assert_eq!(
                spec(kind).presentation_category,
                SlashPresentationCategory::Account
            );
            assert!(!spec(kind).accepts_payload());
        }
    }

    const UPSTREAM_REGISTRY: &[&str] = &[
        "/help · General · show available slash commands",
        "/clear · General · start a fresh conversation while keeping managed processes",
        "/new · Session · start a fresh session",
        "/reset · Session · reset the current session context",
        "/resume · Session · resume a saved session",
        "/rename · Session · rename the current session",
        "/login · Account · choose the model provider and how it signs in",
        "/logout · Account · sign out of a provider session",
        "/provider (/setup) · Account · choose the model provider and how it signs in",
        "/stats · Account · show token and turn statistics",
        "/usage (/cost) · Account · show local oh-fx tokens, models, and spend",
        "/status · General · show runtime configuration",
        "/model · Model · choose what model and reasoning effort to use",
        "/permissions · Security · choose what oh-fx is allowed to do",
        "/allowlist · Security · manage trusted commands, tools, and URLs",
        "/undo · Session · undo the latest tracked file operation",
        "/mcp · Extensions · manage local and remote MCP servers, resources, prompts, and project trust",
        "/skills · Extensions · browse and manage skills",
        "/copy · Session · copy the last assistant response",
        "/compact · Session · summarize context into a fresh window",
        "/settings · Appearance · browse and update settings",
        "/alias · Extensions · show alias availability",
        "/fast · Model · toggle Fast mode when supported",
        "/ultrafast · Model · request Ultra mode when supported",
        "/statusline · Appearance · toggle status line segments",
        "/workspace · Workspace · manage additional workspace directories",
        "/shell · Workspace · reload shell startup files for commands",
        "/version · General · show the oh-fx version",
        "/quit (/exit) · General · exit the interactive shell",
    ];

    fn registry_row(spec: &SlashSpec) -> String {
        let aliases: Vec<String> = spec
            .aliases
            .iter()
            .map(|alias| format!(" ({alias})"))
            .collect();
        format!(
            "{}{} · {} · {}",
            spec.command,
            aliases.concat(),
            spec.presentation_category.label(),
            spec.completion_description
        )
    }

    #[test]
    fn the_registry_keeps_upstreams_order_aliases_categories_and_descriptions() {
        let rows: Vec<String> = SLASH_REGISTRY.commands().iter().map(registry_row).collect();
        assert_eq!(rows, UPSTREAM_REGISTRY);
        assert!(spec(SlashKind::Statusline).accepts_payload());
        assert!(spec(SlashKind::Shell).accepts_payload());
        assert!(spec(SlashKind::Ultrafast).accepts_payload());
    }

    #[test]
    fn help_entries_follow_upstreams_and_name_the_commands_that_take_arguments() {
        let entries: Vec<(&str, &str, bool)> = SLASH_REGISTRY
            .commands()
            .iter()
            .filter(|spec| spec.help_entry != spec.command)
            .map(|spec| (spec.command, spec.help_entry, spec.accepts_payload()))
            .collect();
        assert_eq!(
            entries,
            [
                ("/rename", "/rename <title>", true),
                ("/logout", "/logout [vercel|codex|grok]", true),
                ("/usage", "/usage (/cost)", false),
                ("/model", "/model <id-or-query>", true),
                (
                    "/permissions",
                    "/permissions [ask|auto|full-access|reset]",
                    true
                ),
                (
                    "/allowlist",
                    "/allowlist [view [effective|local|user]|[local|user] add|remove|reset ...]",
                    true
                ),
                (
                    "/mcp",
                    "/mcp [list|resource|prompt|add|remove|path|reload|auth|logout|trust]",
                    true
                ),
                (
                    "/skills",
                    "/skills [list|add|install|show|create|remove|path] [name|url|path] ($ opens skill search)",
                    true
                ),
                ("/settings", "/settings [startup-scrollback [on|off]]", true),
                ("/alias", "/alias [name] [command]", true),
                ("/ultrafast", "/ultrafast [on|off|status]", true),
                (
                    "/statusline",
                    "/statusline [context|session|workspace]",
                    true
                ),
                (
                    "/workspace",
                    "/workspace [list|add PATH|remove PATH|clear]",
                    true
                ),
                ("/shell", "/shell reload", true),
            ]
        );
        assert!(
            SLASH_REGISTRY
                .commands()
                .iter()
                .filter(|spec| spec.help_entry == spec.command)
                .all(|spec| !spec.accepts_payload())
        );
    }

    #[test]
    fn presentation_categories_keep_upstreams_order_and_labels() {
        let labels: Vec<&str> = SlashPresentationCategory::ALL
            .iter()
            .map(|category| category.label())
            .collect();
        assert_eq!(
            labels,
            [
                "General",
                "Session",
                "Account",
                "Model",
                "Appearance",
                "Security",
                "Workspace",
                "Media",
                "Agents",
                "Extensions",
                "Product"
            ]
        );
        for (index, category) in SlashPresentationCategory::ALL.into_iter().enumerate() {
            assert_eq!(category as usize, index);
        }
    }
}
