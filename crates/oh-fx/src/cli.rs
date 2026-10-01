use std::ffi::OsString;

pub(crate) const ASK_USAGE: &str = "usage: oh-fx ask [--model <id>] [--json] [--] <prompt>";

const UPGRADE_USAGE: &str = "usage: oh-fx upgrade [--json]";

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

pub(crate) fn upgrade_json(args: &[OsString]) -> Result<bool, &'static str> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--json" => Ok(true),
        _ => Err(UPGRADE_USAGE),
    }
}

impl AskArguments {
    pub(crate) fn new(args: Vec<OsString>) -> Self {
        Self(args)
    }

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

    fn words(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn ask_arguments(words_after_ask: &[&str]) -> AskArguments {
        AskArguments::new(words(words_after_ask))
    }

    #[test]
    fn upgrade_accepts_one_json_flag() {
        assert_eq!(upgrade_json(&[]), Ok(false));
        assert_eq!(upgrade_json(&words(&["--json"])), Ok(true));
        for rejected in [
            &["--json", "--json"][..],
            &["--channel", "dev"],
            &["--background"],
        ] {
            assert_eq!(upgrade_json(&words(rejected)), Err(UPGRADE_USAGE));
        }
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
}
