use std::ffi::OsString;

use lexopt::Arg::{Long, Short, Value};

pub(crate) const HELP: &str = "oh-fx

Usage:
  oh-fx ask [--model <id>] [--json] <prompt>  Run one noninteractive request
  oh-fx upgrade [--json]                      Upgrade oh-fx to the latest release
  oh-fx --version                             Print the version
  oh-fx help                                  Show this help
";

pub(crate) const ASK_USAGE: &str = "usage: oh-fx ask [--model <id>] [--json] [--] <prompt>";

pub(crate) const ASK_HELP: &str = "oh-fx ask

Run one noninteractive request

Usage:
  oh-fx ask [--model <id>] [--json] [--] <prompt>

Options:
  --model <id>  Override the model for this request
  --json        Emit machine-readable JSON instead of text
  --            Treat every following argument as prompt text

The prompt may be passed as arguments or piped on stdin when no prompt args are given.
Operational progress and diagnostics are written to stderr. JSON `output` keeps accumulated assistant Markdown; `final_output` contains only the completed final response, or an empty string when absent.
JSON usage sums reported main-agent input_tokens and output_tokens; unreported counts are null.
";

pub(crate) const UPGRADE_HELP: &str = "oh-fx upgrade

Upgrade oh-fx to the latest release

Usage:
  oh-fx upgrade [--json]

Options:
  --json  Emit machine-readable JSON instead of text
";

const UPGRADE_USAGE: &str = "usage: oh-fx upgrade [--json]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Help,
    Version,
    Upgrade(UpgradeOptions),
    UpgradeHelp,
    Ask(AskArguments),
    AskHelp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AskArguments(Vec<OsString>);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AskOptions {
    pub(crate) model: Option<String>,
    pub(crate) json: bool,
    pub(crate) prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskArgsError {
    InvalidAskArgs,
    InvalidPromptText,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpgradeOptions {
    pub(crate) json: bool,
    pub(crate) background: bool,
}

pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let args: Vec<OsString> = args.into_iter().collect();
    if let Some((first, rest)) = args.split_first()
        && first == "ask"
    {
        return Ok(parse_ask(rest));
    }
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

fn parse_ask(args: &[OsString]) -> Command {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Command::AskHelp;
    }
    Command::Ask(AskArguments(args.to_vec()))
}

impl AskArguments {
    pub(crate) fn requests_json(&self) -> bool {
        self.0
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--json")
    }

    pub(crate) fn options(&self) -> Result<AskOptions, AskArgsError> {
        parse_ask_options(&self.0)
    }
}

fn parse_ask_options(args: &[OsString]) -> Result<AskOptions, AskArgsError> {
    let mut options = AskOptions::default();
    let mut words: Vec<&OsString> = Vec::new();
    let mut options_ended = false;
    let mut remaining = args.iter();
    while let Some(arg) = remaining.next() {
        if options_ended {
            words.push(arg);
            continue;
        }
        match arg.to_str() {
            Some("--") => options_ended = true,
            Some("--json") => options.json = true,
            Some("--model") => {
                let model = remaining
                    .next()
                    .and_then(|value| value.to_str())
                    .map(|value| value.trim_matches([' ', '\t', '\r', '\n']))
                    .filter(|value| !value.is_empty())
                    .ok_or(AskArgsError::InvalidAskArgs)?;
                options.model = Some(model.to_owned());
            }
            Some(flag) if flag.len() > 1 && flag.starts_with('-') => {
                return Err(AskArgsError::InvalidAskArgs);
            }
            _ => words.push(arg),
        }
    }
    if !words.is_empty() {
        let words: Option<Vec<&str>> = words.iter().map(|word| word.to_str()).collect();
        options.prompt = Some(words.ok_or(AskArgsError::InvalidPromptText)?.join(" "));
    }
    Ok(options)
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

    fn ask_words(words: &[&str]) -> Command {
        parse_words(&[&["ask"], words].concat()).unwrap()
    }

    fn ask_arguments(words: &[&str]) -> AskArguments {
        let Command::Ask(arguments) = ask_words(words) else {
            panic!("expected an ask command");
        };
        arguments
    }

    #[test]
    fn parses_ask_prompts_models_and_json() {
        let arguments = ask_arguments(&["--model", " gpt ", "--json", "hello", "world"]);
        assert!(arguments.requests_json());
        assert_eq!(
            arguments.options(),
            Ok(AskOptions {
                model: Some("gpt".to_owned()),
                json: true,
                prompt: Some("hello world".to_owned()),
            })
        );
        let arguments = ask_arguments(&["--", "--json", "-x"]);
        assert!(!arguments.requests_json());
        assert_eq!(
            arguments.options(),
            Ok(AskOptions {
                prompt: Some("--json -x".to_owned()),
                ..AskOptions::default()
            })
        );
        assert_eq!(ask_arguments(&[]).options(), Ok(AskOptions::default()));
    }

    #[test]
    fn a_json_model_name_is_a_value_not_the_output_mode() {
        let arguments = ask_arguments(&["--model", "--json", "hi"]);
        assert!(arguments.requests_json());
        let options = arguments.options().unwrap();
        assert!(!options.json);
        assert_eq!(options.model.as_deref(), Some("--json"));
    }

    #[test]
    fn rejects_unknown_ask_flags_and_missing_models() {
        for words in [
            &["--quiet", "hi"][..],
            &["--model"],
            &["--model", "  ", "hi"],
            &["-x"],
        ] {
            assert_eq!(
                ask_arguments(words).options(),
                Err(AskArgsError::InvalidAskArgs)
            );
        }
        let arguments = ask_arguments(&["--json", "--bogus"]);
        assert!(arguments.requests_json());
        assert_eq!(arguments.options(), Err(AskArgsError::InvalidAskArgs));
    }

    #[test]
    fn ask_help_wins_anywhere_after_the_command() {
        assert_eq!(ask_words(&["hello", "--help"]), Command::AskHelp);
        assert_eq!(ask_words(&["--", "-h"]), Command::AskHelp);
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
