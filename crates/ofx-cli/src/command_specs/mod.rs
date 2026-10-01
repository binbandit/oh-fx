mod top_level_help;

pub use top_level_help::{
    HelpStyle, TOP_LEVEL_HELP_DEFAULT_WIDTH, parse_column_count, render_command_help,
    render_top_level_help,
};

use crate::commands::TOP_LEVEL_SPECS;

pub(crate) const PRODUCT_NAME: &str = "oh-fx";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopLevelKind {
    Help,
    Ask,
    Acp,
    Pr,
    Issue,
    Login,
    Logout,
    Setup,
    Status,
    Permissions,
    Mcp,
    Models,
    Provider,
    Doctor,
    Teams,
    Session,
    Sessions,
    Resume,
    Credits,
    Usage,
    Upgrade,
    Replay,
    Workspace,
}

impl TopLevelKind {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 23] = [
        Self::Help,
        Self::Ask,
        Self::Acp,
        Self::Pr,
        Self::Issue,
        Self::Login,
        Self::Logout,
        Self::Setup,
        Self::Status,
        Self::Permissions,
        Self::Mcp,
        Self::Models,
        Self::Provider,
        Self::Doctor,
        Self::Teams,
        Self::Session,
        Self::Sessions,
        Self::Resume,
        Self::Credits,
        Self::Usage,
        Self::Upgrade,
        Self::Replay,
        Self::Workspace,
    ];

    pub(crate) fn spec(self) -> &'static TopLevelSpec {
        &TOP_LEVEL_SPECS[self as usize]
    }

    pub fn token(self) -> &'static str {
        self.spec().token
    }

    pub(crate) fn from_token(token: &str) -> Option<Self> {
        TOP_LEVEL_SPECS
            .iter()
            .find(|spec| spec.matches(token))
            .map(|spec| spec.kind)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OptionDoc {
    pub(crate) flag: &'static str,
    pub(crate) description: &'static str,
}

impl OptionDoc {
    pub(crate) const fn new(flag: &'static str, description: &'static str) -> Self {
        Self { flag, description }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TopLevelSpec {
    pub(crate) kind: TopLevelKind,
    pub(crate) token: &'static str,
    pub(crate) aliases: &'static [&'static str],
    pub(crate) usage: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) options: &'static [OptionDoc],
    pub(crate) details: &'static [&'static str],
    pub(crate) hidden_from_top_level_help: bool,
}

impl TopLevelSpec {
    pub(crate) const fn new(
        kind: TopLevelKind,
        token: &'static str,
        usage: &'static str,
        summary: &'static str,
    ) -> Self {
        Self {
            kind,
            token,
            aliases: &[],
            usage,
            summary,
            options: &[],
            details: &[],
            hidden_from_top_level_help: false,
        }
    }

    pub(crate) const fn with_aliases(self, aliases: &'static [&'static str]) -> Self {
        Self { aliases, ..self }
    }

    pub(crate) const fn with_options(self, options: &'static [OptionDoc]) -> Self {
        Self { options, ..self }
    }

    pub(crate) const fn with_details(self, details: &'static [&'static str]) -> Self {
        Self { details, ..self }
    }

    pub(crate) const fn hidden(self) -> Self {
        Self {
            hidden_from_top_level_help: true,
            ..self
        }
    }

    pub(crate) fn tokens(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.token).chain(self.aliases.iter().copied())
    }

    pub(crate) fn matches(&self, input: &str) -> bool {
        self.tokens()
            .any(|token| matches_command_token(input, token))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TopLevelHelpEntry {
    Command {
        kind: TopLevelKind,
        usage: &'static str,
        summary: Option<&'static str>,
    },
    Extra {
        usage: &'static str,
        summary: &'static str,
    },
}

impl TopLevelHelpEntry {
    pub(crate) const fn command(kind: TopLevelKind, usage: &'static str) -> Self {
        Self::Command {
            kind,
            usage,
            summary: None,
        }
    }

    pub(crate) const fn summarized(
        kind: TopLevelKind,
        usage: &'static str,
        summary: &'static str,
    ) -> Self {
        Self::Command {
            kind,
            usage,
            summary: Some(summary),
        }
    }

    pub(crate) const fn extra(usage: &'static str, summary: &'static str) -> Self {
        Self::Extra { usage, summary }
    }

    pub(crate) fn kind(&self) -> Option<TopLevelKind> {
        match self {
            Self::Command { kind, .. } => Some(*kind),
            Self::Extra { .. } => None,
        }
    }

    pub(crate) fn usage(&self) -> &'static str {
        match self {
            Self::Command { usage, .. } | Self::Extra { usage, .. } => usage,
        }
    }

    fn summary(&self) -> &'static str {
        match self {
            Self::Command { kind, summary, .. } => summary.unwrap_or(kind.spec().summary),
            Self::Extra { summary, .. } => summary,
        }
    }

    fn is_hidden(&self) -> bool {
        self.kind()
            .is_some_and(|kind| kind.spec().hidden_from_top_level_help)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TopLevelFlag {
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TopLevelExample {
    pub(crate) command: &'static str,
    pub(crate) description: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TopLevelResource {
    pub(crate) label: &'static str,
    pub(crate) value: &'static str,
    pub(crate) link: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TopLevelHelp {
    pub(crate) description: &'static str,
    pub(crate) interactive_hint: &'static str,
    pub(crate) help_groups: &'static [&'static [TopLevelHelpEntry]],
    pub(crate) flags: &'static [TopLevelFlag],
    pub(crate) examples: &'static [TopLevelExample],
    pub(crate) notes: &'static [&'static str],
    pub(crate) resources: &'static [TopLevelResource],
}

fn matches_command_token(input: &str, token: &str) -> bool {
    input.trim_end_matches([' ', '\t']) == token
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::TOP_LEVEL_HELP;

    #[test]
    fn top_level_matcher_recognizes_help_aliases() {
        let help = TopLevelKind::Help.spec();
        assert!(help.matches("help"));
        assert!(help.matches("--help"));
        assert!(help.matches("-h"));
        assert!(!help.matches("wat"));
    }

    #[test]
    fn top_level_matcher_trims_only_trailing_spaces_and_tabs() {
        let help = TopLevelKind::Help.spec();
        assert!(help.matches("help \t"));
        assert!(!help.matches(" help"));
        assert!(!help.matches("help\n"));
        assert_eq!(
            TopLevelKind::from_token("balance "),
            Some(TopLevelKind::Credits)
        );
    }

    #[test]
    fn top_level_specs_are_ordered_by_kind() {
        for (index, kind) in TopLevelKind::ALL.into_iter().enumerate() {
            assert_eq!(kind as usize, index);
            assert_eq!(kind.spec().kind, kind);
            assert!(!kind.spec().usage.is_empty());
        }
        assert_eq!(TOP_LEVEL_SPECS.len(), TopLevelKind::ALL.len());
    }

    #[test]
    fn top_level_specs_keep_unique_tokens_and_aliases() {
        let tokens = || TOP_LEVEL_SPECS.iter().flat_map(TopLevelSpec::tokens);
        for token in tokens() {
            assert_eq!(tokens().filter(|candidate| *candidate == token).count(), 1);
        }
    }

    #[test]
    fn top_level_help_index_covers_visible_commands_once() {
        for spec in TOP_LEVEL_SPECS {
            let expected = usize::from(!spec.hidden_from_top_level_help);
            let count = TOP_LEVEL_HELP
                .help_groups
                .iter()
                .flat_map(|group| group.iter())
                .filter(|entry| entry.kind() == Some(spec.kind))
                .count();
            assert_eq!(count, expected, "{:?}", spec.kind);
        }
    }

    #[test]
    fn every_command_help_matches_its_golden_file() {
        for kind in TopLevelKind::ALL {
            if kind == TopLevelKind::Help {
                continue;
            }
            let path = format!(
                "{}/tests/golden/command_{}.txt",
                env!("CARGO_MANIFEST_DIR"),
                kind.spec().token
            );
            let golden =
                std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
            assert_eq!(render_command_help(kind), golden, "{kind:?}");
        }
    }
}
