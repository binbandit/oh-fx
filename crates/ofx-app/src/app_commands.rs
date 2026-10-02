use ofx_cli::{SLASH_REGISTRY, SlashKind};
use ofx_contract::{NoticeTone, UiEvent};
use ofx_session::resolve_model_query_from_ids;
use ofx_tui::SlashCommandSpec;

use crate::app_agent_runtime::ControllerState;

const UNKNOWN_COMMAND: &str = "Unknown command. Try /help.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandEffect {
    None,
    SwitchModel(String),
    Clear,
}

pub(crate) fn slash_command_specs() -> Vec<SlashCommandSpec> {
    SLASH_REGISTRY
        .commands()
        .iter()
        .map(|spec| SlashCommandSpec {
            command: spec.command.to_owned(),
            aliases: spec
                .aliases
                .iter()
                .map(|alias| (*alias).to_owned())
                .collect(),
            description: spec.completion_description.to_owned(),
        })
        .collect()
}

pub(crate) fn handle_command(
    state: &ControllerState,
    text: &str,
    turn_active: bool,
) -> CommandEffect {
    let Some(command) = SLASH_REGISTRY.parse_command(text) else {
        state.notice(NoticeTone::Error, "command", UNKNOWN_COMMAND);
        return CommandEffect::None;
    };
    match command.kind {
        SlashKind::Quit => {
            state.emit(UiEvent::ExitRequested);
            CommandEffect::None
        }
        SlashKind::Help => {
            state.emit(UiEvent::HelpRequested);
            CommandEffect::None
        }
        SlashKind::ClearScreen => CommandEffect::Clear,
        SlashKind::Model if command.payload.is_empty() => {
            state.notice(NoticeTone::Neutral, "model", &model_status(state));
            CommandEffect::None
        }
        SlashKind::Permissions => {
            state.permissions().handle_command(command.payload);
            CommandEffect::None
        }
        SlashKind::Model => {
            let resolved = resolve_model_query(state.models(), command.payload);
            let prefix = if turn_active {
                "Next turn will use "
            } else {
                "Switched to "
            };
            state.notice(NoticeTone::Neutral, "", &format!("{prefix}{resolved}"));
            CommandEffect::SwitchModel(resolved)
        }
    }
}

fn model_status(state: &ControllerState) -> String {
    let models = state.models();
    if models.is_empty() {
        return state.model().to_owned();
    }
    format!("{}\navailable: {}", state.model(), models.join(", "))
}

fn resolve_model_query(ids: &[String], query: &str) -> String {
    resolve_model_query_from_ids(ids, query)
        .unwrap_or(query)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_lists_the_registry_commands_with_their_aliases() {
        let specs = slash_command_specs();
        let commands: Vec<&str> = specs.iter().map(|spec| spec.command.as_str()).collect();
        assert_eq!(
            commands,
            ["/help", "/clear", "/model", "/permissions", "/quit"]
        );
        assert_eq!(specs[4].aliases, ["/exit"]);
        assert_eq!(specs[4].description, "exit the interactive shell");
    }

    #[test]
    fn unmatched_model_queries_are_used_as_given() {
        let models = vec!["openai/gpt-5".to_owned()];
        assert_eq!(resolve_model_query(&models, "gpt-5"), "openai/gpt-5");
        assert_eq!(resolve_model_query(&models, "zzz"), "zzz");
        assert_eq!(resolve_model_query(&[], "anything"), "anything");
    }
}
