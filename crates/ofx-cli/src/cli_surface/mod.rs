mod failure;

use std::ffi::OsString;

use crate::command_specs::TopLevelKind;

pub use failure::CliError;

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum Invocation {
    Interactive,
    TopLevelHelp,
    CommandHelp(TopLevelKind),
    Version,
    Command(TopLevelKind, Vec<OsString>),
}

pub fn parse_args<I, T>(args: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let Some(first) = args.next() else {
        return Ok(Invocation::Interactive);
    };
    let rest: Vec<OsString> = args.collect();
    match first.to_str().and_then(TopLevelKind::from_token) {
        Some(TopLevelKind::Help) => Ok(Invocation::TopLevelHelp),
        Some(kind) if requests_command_help(&rest) => Ok(Invocation::CommandHelp(kind)),
        Some(kind @ (TopLevelKind::Mcp | TopLevelKind::Slack)) if rest.is_empty() => {
            Ok(Invocation::CommandHelp(kind))
        }
        Some(kind) => Ok(Invocation::Command(kind, rest)),
        None if first != "--version" && first != "-v" => Err(CliError::UnknownSubcommand(first)),
        None if rest.is_empty() => Ok(Invocation::Version),
        None => Err(CliError::VersionUsage),
    }
}

fn requests_command_help(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == "--help" || arg == "-h")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_specs::{HelpStyle, TOP_LEVEL_HELP_DEFAULT_WIDTH, render_top_level_help};

    fn parse(args: &[&str]) -> Result<Invocation, CliError> {
        parse_args(args.iter().copied())
    }

    fn stderr(args: &[&str]) -> String {
        parse(args).unwrap_err().report("0.0.0")
    }

    #[test]
    fn no_arguments_launch_the_interactive_session() {
        assert_eq!(parse(&[]), Ok(Invocation::Interactive));
    }

    #[test]
    fn help_aliases_route_to_help() {
        for alias in ["help", "--help", "-h", "help \t"] {
            assert_eq!(parse(&[alias, "ignored"]), Ok(Invocation::TopLevelHelp));
        }
    }

    #[test]
    fn commands_and_aliases_keep_their_arguments() {
        assert_eq!(
            parse(&["ask", "hello", "world"]),
            Ok(Invocation::Command(
                TopLevelKind::Ask,
                vec![OsString::from("hello"), OsString::from("world")]
            ))
        );
        assert_eq!(
            parse(&["balance"]),
            Ok(Invocation::Command(TopLevelKind::Credits, Vec::new()))
        );
        assert_eq!(
            parse(&["-c"]),
            Ok(Invocation::Command(TopLevelKind::Resume, Vec::new()))
        );
    }

    #[test]
    fn per_command_help_wins_over_command_arguments() {
        assert_eq!(
            parse(&["ask", "--help"]),
            Ok(Invocation::CommandHelp(TopLevelKind::Ask))
        );
        assert_eq!(
            parse(&["ask", "--", "-h"]),
            Ok(Invocation::CommandHelp(TopLevelKind::Ask))
        );
        assert_eq!(
            parse(&["credits", "-h", "extra"]),
            Ok(Invocation::CommandHelp(TopLevelKind::Credits))
        );
        assert_eq!(
            parse(&["-c", "--help"]),
            Ok(Invocation::CommandHelp(TopLevelKind::Resume))
        );
    }

    #[test]
    fn mcp_and_slack_without_arguments_print_their_help() {
        assert_eq!(
            parse(&["mcp"]),
            Ok(Invocation::CommandHelp(TopLevelKind::Mcp))
        );
        assert_eq!(
            parse(&["slack"]),
            Ok(Invocation::CommandHelp(TopLevelKind::Slack))
        );
    }

    #[test]
    fn version_flags_reject_extra_arguments() {
        assert_eq!(parse(&["--version"]), Ok(Invocation::Version));
        assert_eq!(parse(&["-v"]), Ok(Invocation::Version));
        for args in [&["--version", "extra"][..], &["-v", "extra"]] {
            assert_eq!(stderr(args), "usage: oh-fx --version\n");
        }
    }

    #[test]
    fn unknown_commands_print_the_plain_help_at_the_default_width() {
        let help = render_top_level_help(TOP_LEVEL_HELP_DEFAULT_WIDTH, "0.0.0", HelpStyle::Plain);
        for unknown in ["wat", "version", "--record", "-cr"] {
            assert_eq!(
                stderr(&[unknown]),
                format!("oh-fx: unknown subcommand: {unknown}\n\n{help}")
            );
        }
    }
}
