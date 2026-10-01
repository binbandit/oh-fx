use std::ffi::OsString;

use lexopt::Arg::{Long, Short, Value};

pub(crate) const HELP: &str = "oh-fx

Usage:
  oh-fx upgrade [--json]  Upgrade oh-fx to the latest release
  oh-fx --version         Print the version
  oh-fx help              Show this help
";

pub(crate) const UPGRADE_HELP: &str = "oh-fx upgrade

Upgrade oh-fx to the latest release

Usage:
  oh-fx upgrade [--json]

Options:
  --json  Emit machine-readable JSON instead of text
";

const UPGRADE_USAGE: &str = "usage: oh-fx upgrade [--json]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Help,
    Version,
    Upgrade(UpgradeOptions),
    UpgradeHelp,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpgradeOptions {
    pub(crate) json: bool,
    pub(crate) background: bool,
}

pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut parser = lexopt::Parser::from_args(args);
    let first = parser.next().map_err(|error| format!("oh-fx: {error}"))?;
    let command = match first {
        None | Some(Long("help") | Short('h')) => Command::Help,
        Some(Long("version") | Short('v')) => Command::Version,
        Some(Value(command)) if command == "help" => Command::Help,
        Some(Value(command)) if command == "upgrade" => return parse_upgrade(&mut parser),
        Some(other) => return Err(format!("oh-fx: {}", other.unexpected())),
    };
    match parser.next().map_err(|error| format!("oh-fx: {error}"))? {
        None => Ok(command),
        Some(extra) => Err(format!("oh-fx: {}", extra.unexpected())),
    }
}

fn parse_upgrade(parser: &mut lexopt::Parser) -> Result<Command, String> {
    let mut options = UpgradeOptions::default();
    while let Some(arg) = parser.next().map_err(|_| UPGRADE_USAGE.to_owned())? {
        match arg {
            Long("help") | Short('h') => return Ok(Command::UpgradeHelp),
            Long("json") if !options.json => options.json = true,
            Long("background") if !options.background => options.background = true,
            _ => return Err(UPGRADE_USAGE.to_owned()),
        }
    }
    Ok(Command::Upgrade(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> Result<Command, String> {
        parse(words.iter().map(OsString::from))
    }

    #[test]
    fn defaults_to_help() {
        assert_eq!(parse_words(&[]), Ok(Command::Help));
        assert_eq!(parse_words(&["help"]), Ok(Command::Help));
        assert_eq!(parse_words(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn parses_version_flags() {
        assert_eq!(parse_words(&["--version"]), Ok(Command::Version));
        assert_eq!(parse_words(&["-v"]), Ok(Command::Version));
    }

    #[test]
    fn rejects_trailing_arguments() {
        assert!(parse_words(&["--version", "junk"]).is_err());
        assert!(parse_words(&["--version=junk"]).is_err());
        assert!(parse_words(&["help", "--bogus"]).is_err());
    }

    #[test]
    fn parses_upgrade_options_once_each() {
        assert_eq!(
            parse_words(&["upgrade", "--json"]),
            Ok(Command::Upgrade(UpgradeOptions {
                json: true,
                background: false
            }))
        );
        assert_eq!(
            parse_words(&["upgrade", "--json", "--json"]),
            Err(UPGRADE_USAGE.to_owned())
        );
        assert_eq!(
            parse_words(&["upgrade", "--channel", "dev"]),
            Err(UPGRADE_USAGE.to_owned())
        );
    }

    #[test]
    fn shows_upgrade_help() {
        assert_eq!(
            parse_words(&["upgrade", "--help"]),
            Ok(Command::UpgradeHelp)
        );
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(parse_words(&["frobnicate"]).is_err());
    }
}
