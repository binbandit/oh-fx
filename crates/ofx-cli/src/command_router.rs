use crate::command_specs::{SlashKind, SlashSpec};
use crate::registry::SlashRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand<'a> {
    pub kind: SlashKind,
    pub payload: &'a str,
}

impl SlashSpec {
    fn route<'i>(&self, input: &'i str) -> Option<SlashCommand<'i>> {
        let payload = if self.accepts_payload() {
            let token = self.prefix_token(input)?;
            input[token.len()..].trim_matches([' ', '\t'])
        } else {
            self.exact_token(input)?;
            ""
        };
        Some(SlashCommand {
            kind: self.kind,
            payload,
        })
    }
}

impl SlashRegistry<'_> {
    pub fn parse_command<'i>(&self, input: &'i str) -> Option<SlashCommand<'i>> {
        self.commands().iter().find_map(|spec| spec.route(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::SLASH_REGISTRY;

    fn parse(input: &str) -> Option<(SlashKind, &str)> {
        SLASH_REGISTRY
            .parse_command(input)
            .map(|command| (command.kind, command.payload))
    }

    #[test]
    fn parse_extracts_model_command_payload() {
        assert_eq!(
            parse("/model claude-opus"),
            Some((SlashKind::Model, "claude-opus"))
        );
    }

    #[test]
    fn parse_rejects_removed_plural_model_command() {
        assert_eq!(parse("/models"), None);
    }

    #[test]
    fn parse_treats_unknown_and_malformed_command_inputs_as_unknown() {
        for input in [
            "/wat",
            "/output",
            "/bogus",
            "hello",
            "/help me",
            "/clear all",
            " /model claude-opus",
        ] {
            assert_eq!(parse(input), None, "{input}");
        }
    }

    #[test]
    fn parse_tolerates_trailing_whitespace_on_exact_match_commands() {
        assert_eq!(parse("/exit "), Some((SlashKind::Quit, "")));
        assert_eq!(parse("/quit  "), Some((SlashKind::Quit, "")));
        assert_eq!(parse("/exit \t"), Some((SlashKind::Quit, "")));
        assert_eq!(parse("/help "), Some((SlashKind::Help, "")));
        assert_eq!(parse("/clear\t"), Some((SlashKind::ClearScreen, "")));
    }

    #[test]
    fn parse_takes_the_ported_commands_without_a_payload() {
        for (input, kind) in [
            ("/reset", SlashKind::ResetSession),
            ("/stats", SlashKind::Stats),
            ("/status", SlashKind::Status),
            ("/undo", SlashKind::Undo),
            ("/copy", SlashKind::Copy),
            ("/compact", SlashKind::Compact),
            ("/fast", SlashKind::Fast),
            ("/version", SlashKind::Version),
        ] {
            assert_eq!(parse(input), Some((kind, "")), "{input}");
            assert_eq!(parse(&format!("{input}\t ")), Some((kind, "")), "{input}");
            assert_eq!(parse(&format!("{input} now")), None, "{input}");
        }
    }

    #[test]
    fn parse_returns_empty_payload_for_bare_prefix_commands() {
        assert_eq!(parse("/model"), Some((SlashKind::Model, "")));
        assert_eq!(parse("/mcp"), Some((SlashKind::Mcp, "")));
    }

    #[test]
    fn parse_hands_the_mcp_subcommand_through_as_its_payload() {
        assert_eq!(parse("/mcp list"), Some((SlashKind::Mcp, "list")));
        assert_eq!(
            parse("/mcp\ttrust approve docs "),
            Some((SlashKind::Mcp, "trust approve docs"))
        );
        assert_eq!(parse("/mcps"), None);
    }

    #[test]
    fn parse_trims_only_spaces_and_tabs_around_payload() {
        assert_eq!(
            parse("/model \t claude-opus \t"),
            Some((SlashKind::Model, "claude-opus"))
        );
    }

    #[test]
    fn parse_covers_every_registered_slash_command_token_and_alias() {
        for spec in SLASH_REGISTRY.commands() {
            for token in spec.tokens() {
                assert_eq!(
                    parse(token).map(|(kind, _)| kind),
                    Some(spec.kind),
                    "{token}"
                );
            }
        }
    }

    #[test]
    fn parse_payload_acceptance_follows_slash_spec_metadata() {
        for spec in SLASH_REGISTRY.commands() {
            let input = format!("{} sample", spec.command);
            assert_eq!(parse(&input).is_some(), spec.accepts_payload(), "{input}");
        }
    }

    #[test]
    fn route_forwards_borrowed_payload_slice() {
        let input = "/model claude-opus";
        let Some((SlashKind::Model, payload)) = parse(input) else {
            panic!("expected a model command");
        };
        assert_eq!(payload, "claude-opus");
        assert!(std::ptr::eq(
            payload.as_ptr(),
            input["/model ".len()..].as_ptr()
        ));
    }
}
