use ofx_contract::UiCommand;

use super::{MAX_PROMPT_HISTORY, Shell, SlashCommandSpec, Submission, SubmissionState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Submit {
    Empty,
    Command(String),
    Prompt(String),
}

fn command_token(text: &str) -> &str {
    text.split(char::is_whitespace).next().unwrap_or(text)
}

fn lookup<'a>(commands: &'a [SlashCommandSpec], token: &str) -> Option<&'a SlashCommandSpec> {
    commands
        .iter()
        .find(|spec| spec.command == token || spec.aliases.iter().any(|alias| alias == token))
}

fn match_rank(command: &str, prefix: &str) -> Option<usize> {
    if command == prefix {
        return Some(0);
    }
    if command.starts_with(prefix) {
        return Some(1);
    }
    if prefix.len() <= 1 || command.len() <= 1 {
        return None;
    }
    command[1..].contains(&prefix[1..]).then_some(2)
}

fn best_match<'a>(spec: &'a SlashCommandSpec, prefix: &str) -> Option<(usize, &'a str)> {
    let mut best = match_rank(&spec.command, prefix).map(|rank| (rank, spec.command.as_str()));
    for alias in &spec.aliases {
        if let Some(rank) = match_rank(alias, prefix)
            && best.is_none_or(|(current, _)| rank < current)
        {
            best = Some((rank, alias.as_str()));
        }
    }
    best
}

pub(crate) fn slash_completions<'a>(
    commands: &'a [SlashCommandSpec],
    prefix: &str,
) -> Vec<&'a str> {
    if !prefix.starts_with('/') {
        return Vec::new();
    }
    let mut ranked: Vec<(usize, usize, &str)> = commands
        .iter()
        .enumerate()
        .filter_map(|(order, spec)| {
            best_match(spec, prefix).map(|(rank, command)| (rank, order, command))
        })
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, _, command)| command).collect()
}

fn slash_menu_visible(text: &str) -> bool {
    text.starts_with('/') && !text.contains(char::is_whitespace)
}

pub(crate) fn classify(text: &str, commands: &[SlashCommandSpec]) -> Submit {
    let left_trimmed = text.trim_start_matches([' ', '\t', '\r', '\n']);
    let completions = if slash_menu_visible(left_trimmed) {
        slash_completions(commands, left_trimmed)
    } else {
        Vec::new()
    };
    let resolved = completions.first().copied().unwrap_or(left_trimmed);
    if resolved.starts_with('/') && lookup(commands, command_token(resolved)).is_some() {
        return Submit::Command(resolved.to_owned());
    }
    if slash_menu_visible(left_trimmed)
        && !left_trimmed[1..].contains('/')
        && completions.is_empty()
    {
        return Submit::Command(left_trimmed.to_owned());
    }
    let trimmed = text.trim_matches([' ', '\t', '\r', '\n']);
    if trimmed.is_empty() {
        Submit::Empty
    } else {
        Submit::Prompt(trimmed.to_owned())
    }
}

impl Shell<'_> {
    pub(super) fn submit(&mut self) {
        self.dismiss_compaction_feedback();
        let text = self.composer.expanded_text().into_owned();
        match classify(&text, &self.options.commands) {
            Submit::Empty => self.composer.clear(),
            Submit::Command(command) => {
                self.composer.record_history(MAX_PROMPT_HISTORY);
                self.composer.clear();
                self.send(UiCommand::RunCommand { text: command });
            }
            Submit::Prompt(prompt) => {
                self.composer.record_history(MAX_PROMPT_HISTORY);
                self.composer.clear();
                self.outstanding.push_back(Submission {
                    prompt: prompt.clone(),
                    state: SubmissionState::Queued,
                    turn_id: None,
                    sequence: self.submitted_prompts,
                });
                self.submitted_prompts += 1;
                self.send(UiCommand::Submit { prompt });
                self.promote_next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Vec<SlashCommandSpec> {
        [
            ("/help", &[][..]),
            ("/clear", &[]),
            ("/reset", &[]),
            ("/stats", &[]),
            ("/status", &[]),
            ("/model", &[]),
            ("/copy", &[]),
            ("/compact", &[]),
            ("/fast", &[]),
            ("/version", &[]),
            ("/quit", &["/exit"]),
        ]
        .into_iter()
        .map(|(command, aliases)| SlashCommandSpec {
            command: command.to_owned(),
            aliases: aliases
                .iter()
                .map(|alias: &&str| (*alias).to_owned())
                .collect(),
            description: String::new(),
            category: 0,
        })
        .collect()
    }

    #[test]
    fn known_commands_and_aliases_run_locally() {
        let commands = commands();
        assert_eq!(
            classify("/help", &commands),
            Submit::Command("/help".to_owned())
        );
        assert_eq!(
            classify("  /exit", &commands),
            Submit::Command("/exit".to_owned())
        );
        assert_eq!(
            classify("/model gpt-5", &commands),
            Submit::Command("/model gpt-5".to_owned())
        );
    }

    #[test]
    fn partial_commands_resolve_through_the_first_completion() {
        let commands = commands();
        assert_eq!(
            classify("/he", &commands),
            Submit::Command("/help".to_owned())
        );
        assert_eq!(
            classify("/ex", &commands),
            Submit::Command("/exit".to_owned())
        );
        assert_eq!(
            classify("/del", &commands),
            Submit::Command("/model".to_owned())
        );
        assert_eq!(
            slash_completions(&commands, "/"),
            [
                "/help", "/clear", "/reset", "/stats", "/status", "/model", "/copy", "/compact",
                "/fast", "/version", "/quit"
            ]
        );
        assert_eq!(
            classify("/co", &commands),
            Submit::Command("/copy".to_owned())
        );
        assert_eq!(
            classify("/com", &commands),
            Submit::Command("/compact".to_owned())
        );
        assert_eq!(
            classify("/statu", &commands),
            Submit::Command("/status".to_owned())
        );
        assert_eq!(slash_completions(&commands, "/sta"), ["/stats", "/status"]);
        assert_eq!(
            classify("/fa", &commands),
            Submit::Command("/fast".to_owned())
        );
        assert_eq!(
            classify("/st", &commands),
            Submit::Command("/stats".to_owned())
        );
        assert_eq!(
            classify("/cop", &commands),
            Submit::Command("/copy".to_owned())
        );
        assert_eq!(
            classify("/res", &commands),
            Submit::Command("/reset".to_owned())
        );
        assert_eq!(
            classify("/ver", &commands),
            Submit::Command("/version".to_owned())
        );
        assert_eq!(
            slash_completions(&commands, "/e"),
            ["/exit", "/help", "/clear", "/reset", "/model", "/version"]
        );
    }

    #[test]
    fn unknown_single_words_route_as_commands_and_paths_stay_prompts() {
        let commands = commands();
        assert_eq!(
            classify("/bogus", &commands),
            Submit::Command("/bogus".to_owned())
        );
        assert_eq!(
            classify("/tmp/file", &commands),
            Submit::Prompt("/tmp/file".to_owned())
        );
        assert_eq!(
            classify("/tmp/file is broken", &commands),
            Submit::Prompt("/tmp/file is broken".to_owned())
        );
        assert_eq!(
            classify("  hello\n", &commands),
            Submit::Prompt("hello".to_owned())
        );
        assert_eq!(classify(" \n ", &commands), Submit::Empty);
    }
}
