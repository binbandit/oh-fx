use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_cli::{SLASH_REGISTRY, SlashKind, SlashPresentationCategory};
use ofx_contract::{
    CompactionActivity, CompactionEnd, ModelCapabilities, ModelCatalog, ModelOption, Notice,
    NoticeTone, ReasoningEffort, UiEvent,
};
use ofx_session::resolve_model_query_from_ids;
use ofx_text::encode_terminal_safe;
use ofx_tui::SlashCommandSpec;
use ofx_workspace::{ChangeTracker, MAX_PATH_BYTES, UndoResult};

use crate::app_agent_runtime::ControllerState;
use crate::app_session_runtime::{Persistence, RenameError, validate_session_title};
use crate::mcp_commands::handle_mcp;
use crate::session_commands::{handle_allowlist, handle_settings};
use crate::skill_commands::{InstallRequest, handle_skills};

const UNKNOWN_COMMAND: &str = "Unknown command. Try /help.";
const CLIPBOARD_TOPIC: &str = "clipboard";
const NO_REPLY_TO_COPY: &str = "No assistant reply to copy.";
const COPIED: &str = "Copied to clipboard.";
const COPY_FAILED: &str = "Failed to copy to clipboard.";
const FAST_TOPIC: &str = "fast";
const NO_FAST_MODE: &str = "This model does not come with a fast mode.";
const UNDO_TOPIC: &str = "undo";
const USAGE_TOPIC: &str = "usage";
const WORKSPACE_TOPIC: &str = "workspace";
const ALIASES_TOPIC: &str = "aliases";
const ALIASES_UNAVAILABLE: &str = "Aliases are not yet configurable.";
const WORKSPACE_ACCESS_UNAVAILABLE: &str = "Workspace access is unavailable in this runtime.";
const PROFILE_USAGE_UNAVAILABLE: &str =
    "Durable profile usage is unavailable in this host; active session usage remains in memory.";
const NOTHING_TO_UNDO: &str = "Nothing to undo.";
const RESUME_DURING_TURN: &str = "resume is unavailable until the response finishes";
const SESSION_TOPIC: &str = "session";
const RENAME_USAGE: &str = "usage: /rename <title>";
const MODEL_USAGE: &str = "usage: /model <id> <effort> [normal|fast]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandEffect {
    None,
    Install(InstallRequest),
    SwitchModel(String),
    Clear,
    ToggleFast,
    Compact,
    OpenSessions,
    OpenSettings,
    Rename(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelPick {
    pub(crate) model: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) fast_mode: Option<bool>,
}

pub(crate) enum ModelChange {
    Query(String),
    Pick(ModelPick),
    ToggleFast,
}

pub(crate) enum Outcome {
    Unchanged,
    Changed { effort: Option<ReasoningEffort> },
}

impl ModelChange {
    pub(crate) fn needs_catalog(&self, state: &ControllerState) -> bool {
        !matches!(self, Self::ToggleFast) || !state.fast_mode()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Work {
    Idle,
    Turn,
    Compaction,
}

pub(crate) fn refuse_resume_during_turn(state: &ControllerState) {
    state.notice(NoticeTone::Neutral, "session", RESUME_DURING_TURN);
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
            help_entry: spec.help_entry.to_owned(),
            takes_arguments: spec.accepts_payload(),
            category: spec.presentation_category as usize,
            compacts: spec.kind == SlashKind::Compact,
        })
        .collect()
}

pub(crate) fn slash_command_categories() -> Vec<String> {
    SlashPresentationCategory::ALL
        .iter()
        .map(|category| category.label().to_owned())
        .collect()
}

pub(crate) fn handle_command(state: &mut ControllerState, text: &str, work: Work) -> CommandEffect {
    let Some(command) = SLASH_REGISTRY.parse_command(text) else {
        state.notice(NoticeTone::Error, "command", UNKNOWN_COMMAND);
        return CommandEffect::None;
    };
    match command.kind {
        SlashKind::ClearScreen | SlashKind::NewSession | SlashKind::ResetSession => {
            CommandEffect::Clear
        }
        SlashKind::ResumeSession if work != Work::Idle => {
            refuse_resume_during_turn(state);
            CommandEffect::None
        }
        SlashKind::ResumeSession => CommandEffect::OpenSessions,
        SlashKind::RenameSession => CommandEffect::Rename(command.payload.to_owned()),
        SlashKind::Fast => CommandEffect::ToggleFast,
        SlashKind::Settings if command.payload.trim().is_empty() => CommandEffect::OpenSettings,
        SlashKind::Compact => compaction_effect(state, work),
        SlashKind::Skills => handle_skills(state, command.payload)
            .map_or(CommandEffect::None, CommandEffect::Install),
        SlashKind::Model if !command.payload.is_empty() => {
            CommandEffect::SwitchModel(command.payload.to_owned())
        }
        kind => {
            report(state, kind, command.payload);
            CommandEffect::None
        }
    }
}

fn compaction_effect(state: &ControllerState, work: Work) -> CommandEffect {
    match work {
        Work::Compaction => CommandEffect::None,
        _ if !state.has_context_to_compact() => {
            state.compaction(CompactionActivity::Ended(CompactionEnd::NothingToCompact));
            CommandEffect::None
        }
        Work::Idle => CommandEffect::Compact,
        Work::Turn => {
            state.compaction(CompactionActivity::Ended(CompactionEnd::Busy));
            CommandEffect::None
        }
    }
}

fn report(state: &mut ControllerState, kind: SlashKind, payload: &str) {
    match kind {
        SlashKind::Quit => state.emit(UiEvent::ExitRequested),
        SlashKind::Help => state.emit(UiEvent::HelpRequested),
        SlashKind::Usage => {
            state.notice(NoticeTone::Neutral, USAGE_TOPIC, PROFILE_USAGE_UNAVAILABLE);
        }
        SlashKind::Status => state.notice(NoticeTone::Neutral, "status", &state.status_body()),
        SlashKind::Stats => state.emit(UiEvent::StatsRequested),
        SlashKind::Settings => {
            for notice in handle_settings(&state.settings_access(), &state.session_facts(), payload)
            {
                state.emit(UiEvent::Notice { notice });
            }
        }
        SlashKind::Alias => {
            state.notice(NoticeTone::Neutral, ALIASES_TOPIC, ALIASES_UNAVAILABLE);
        }
        SlashKind::Allowlist => state.emit(UiEvent::Notice {
            notice: handle_allowlist(&state.settings_access(), payload),
        }),
        SlashKind::Undo => {
            let result = state
                .change_tracker()
                .map_or(UndoResult::Empty, ChangeTracker::undo_last);
            state.notice(NoticeTone::Neutral, UNDO_TOPIC, &undo_message(&result));
        }
        SlashKind::Copy => copy_last_reply(state),
        SlashKind::Statusline => state.toggle_statusline(payload),
        SlashKind::Workspace => state.notice(
            NoticeTone::Error,
            WORKSPACE_TOPIC,
            WORKSPACE_ACCESS_UNAVAILABLE,
        ),
        SlashKind::Version => state.notice(NoticeTone::Neutral, "version", ofx_upgrade::VERSION),
        SlashKind::Model => state.notice(NoticeTone::Neutral, "model", state.model()),
        SlashKind::Permissions => state.permissions().handle_command(payload),
        SlashKind::Shell => state.permissions().reload_shell(payload),
        SlashKind::Mcp => handle_mcp(state, payload),
        SlashKind::Skills
        | SlashKind::ClearScreen
        | SlashKind::NewSession
        | SlashKind::ResetSession
        | SlashKind::ResumeSession
        | SlashKind::RenameSession
        | SlashKind::Fast
        | SlashKind::Compact => {}
    }
}

pub(crate) fn listed(catalog: &ModelCatalog) -> &[ModelOption] {
    match catalog {
        ModelCatalog::Listed { models, .. } => models,
        ModelCatalog::Failed { .. } => &[],
    }
}

pub(crate) fn change_model(
    state: &mut ControllerState,
    change: ModelChange,
    models: &[ModelOption],
    work: Work,
) -> Outcome {
    match change {
        ModelChange::Query(query) => {
            switch_model(state, &query, models, work);
            Outcome::Changed { effort: None }
        }
        ModelChange::Pick(pick) => pick_model(state, pick, models, work),
        ModelChange::ToggleFast if toggle_fast(state, models) => Outcome::Changed { effort: None },
        ModelChange::ToggleFast => Outcome::Unchanged,
    }
}

fn switch_model(state: &mut ControllerState, query: &str, models: &[ModelOption], work: Work) {
    let ids: Vec<String> = models.iter().map(|option| option.id.clone()).collect();
    let resolved = resolve_model_query(&ids, query);
    state.notice(
        NoticeTone::Neutral,
        "",
        &model_switch_notice(&resolved, work),
    );
    state.select_model(resolved);
}

fn pick_model(
    state: &mut ControllerState,
    pick: ModelPick,
    models: &[ModelOption],
    work: Work,
) -> Outcome {
    let capabilities = capabilities_of(models, &pick.model);
    let effort_offered = match &pick.effort {
        ReasoningEffort::Auto => true,
        ReasoningEffort::Named(name) => capabilities.reasoning_efforts.contains(name),
    };
    if !effort_offered || capabilities.supports_fast_mode != pick.fast_mode.is_some() {
        state.notice(NoticeTone::Error, "", MODEL_USAGE);
        return Outcome::Unchanged;
    }
    state.notice(
        NoticeTone::Neutral,
        "",
        &model_switch_notice(&pick.model, work),
    );
    let effort = (!capabilities.reasoning_efforts.is_empty()).then_some(pick.effort);
    state.apply_pick(pick.model, effort.as_ref(), pick.fast_mode.unwrap_or(false));
    Outcome::Changed { effort }
}

pub(crate) fn capabilities_of(models: &[ModelOption], model: &str) -> ModelCapabilities {
    models
        .iter()
        .find(|option| option.id == model)
        .map(|option| option.capabilities.clone())
        .unwrap_or_default()
}

fn model_switch_notice(model: &str, work: Work) -> String {
    let prefix = if work == Work::Turn {
        "Next turn will use "
    } else {
        "Switched to "
    };
    format!("{prefix}{model}")
}

fn toggle_fast(state: &mut ControllerState, models: &[ModelOption]) -> bool {
    if state.fast_mode() {
        state.set_fast_mode(false);
        state.save_model_preference(FAST_TOPIC, None);
        state.notice(NoticeTone::Neutral, FAST_TOPIC, "off");
        return true;
    }
    if !capabilities_of(models, state.model()).supports_fast_mode {
        state.notice(NoticeTone::Neutral, FAST_TOPIC, NO_FAST_MODE);
        return false;
    }
    state.set_fast_mode(true);
    state.save_model_preference(FAST_TOPIC, None);
    state.notice(NoticeTone::Neutral, FAST_TOPIC, "on");
    true
}

fn undo_message(result: &UndoResult) -> String {
    match result {
        UndoResult::Restored(path) => format!("Restored {}", display_path(path)),
        UndoResult::Deleted(path) => format!("Deleted {} (was newly created)", display_path(path)),
        UndoResult::Unavailable(path) => format!("Could not undo {}", display_path(path)),
        UndoResult::Empty => NOTHING_TO_UNDO.to_owned(),
    }
}

fn display_path(path: &Path) -> String {
    encode_terminal_safe(path.as_os_str().as_bytes(), MAX_PATH_BYTES).text
}

pub(crate) fn rename_session(
    state: &ControllerState,
    persistence: Option<&mut Persistence>,
    raw: &str,
) {
    let renamed = match persistence {
        Some(persistence) => persistence.rename(raw, state.session_title()),
        None => validate_session_title(raw).and(Err(RenameError::NoActiveSession)),
    };
    state.emit(UiEvent::Notice {
        notice: rename_notice(renamed),
    });
}

fn rename_notice(renamed: Result<String, RenameError>) -> Notice {
    let failure = |body: &str| Notice::new(NoticeTone::Error, SESSION_TOPIC, body);
    match renamed {
        Ok(title) => Notice::new(
            NoticeTone::Neutral,
            SESSION_TOPIC,
            format!("renamed to \"{title}\""),
        ),
        Err(RenameError::EmptyTitle) => Notice::new(NoticeTone::Error, "", RENAME_USAGE),
        Err(RenameError::TitleTooLong) => failure("title is too long"),
        Err(RenameError::InvalidTitle) => failure("title must be printable text"),
        Err(RenameError::NoActiveSession) => failure("no active session to rename"),
        Err(RenameError::NotSaved(error)) => Notice::new(
            NoticeTone::Warning,
            SESSION_TOPIC,
            format!("renamed for this process but not saved ({error})"),
        ),
    }
}

fn copy_last_reply(state: &ControllerState) {
    let Some(reply) = state.last_reply() else {
        state.notice(NoticeTone::Neutral, CLIPBOARD_TOPIC, NO_REPLY_TO_COPY);
        return;
    };
    if state.clipboard().copy(reply) {
        state.notice(NoticeTone::Neutral, CLIPBOARD_TOPIC, COPIED);
    } else {
        state.notice(NoticeTone::Error, CLIPBOARD_TOPIC, COPY_FAILED);
    }
}

fn resolve_model_query(ids: &[String], query: &str) -> String {
    resolve_model_query_from_ids(ids, query)
        .unwrap_or(query)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::*;

    fn listed(command: &str) -> SlashCommandSpec {
        slash_command_specs()
            .into_iter()
            .find(|spec| spec.command == command)
            .unwrap_or_else(|| panic!("{command} is not listed"))
    }

    #[test]
    fn the_shell_lists_the_registry_commands_with_their_aliases() {
        let specs = slash_command_specs();
        let shown: Vec<(&str, Vec<&str>, &str, &str, bool)> = specs
            .iter()
            .map(|spec| {
                (
                    spec.command.as_str(),
                    spec.aliases.iter().map(String::as_str).collect(),
                    spec.description.as_str(),
                    spec.help_entry.as_str(),
                    spec.takes_arguments,
                )
            })
            .collect();
        let registered: Vec<(&str, Vec<&str>, &str, &str, bool)> = SLASH_REGISTRY
            .commands()
            .iter()
            .map(|spec| {
                (
                    spec.command,
                    spec.aliases.to_vec(),
                    spec.completion_description,
                    spec.help_entry,
                    spec.accepts_payload(),
                )
            })
            .collect();
        assert_eq!(shown, registered);
        let compacting: Vec<&str> = specs
            .iter()
            .filter(|spec| spec.compacts)
            .map(|spec| spec.command.as_str())
            .collect();
        assert_eq!(compacting, ["/compact"]);
        assert_eq!(listed("/new").description, "start a fresh session");
        assert_eq!(listed("/resume").description, "resume a saved session");
        assert_eq!(listed("/rename").description, "rename the current session");
        assert_eq!(
            listed("/mcp").description,
            "manage local and remote MCP servers, resources, prompts, and project trust"
        );
        assert_eq!(listed("/skills").description, "browse and manage skills");
        assert_eq!(
            listed("/shell").description,
            "reload shell startup files for commands"
        );
        assert_eq!(listed("/quit").aliases, ["/exit"]);
        assert_eq!(listed("/quit").description, "exit the interactive shell");
    }

    #[test]
    fn the_shell_groups_commands_by_upstreams_presentation_categories() {
        let categories = slash_command_categories();
        let specs = slash_command_specs();
        let grouped: Vec<(&str, &str)> = specs
            .iter()
            .map(|spec| (spec.command.as_str(), categories[spec.category].as_str()))
            .collect();
        let registered: Vec<(&str, &str)> = SLASH_REGISTRY
            .commands()
            .iter()
            .map(|spec| (spec.command, spec.presentation_category.label()))
            .collect();
        assert_eq!(grouped, registered);
        assert_eq!(categories.len(), 11);
        assert_eq!(categories[0], "General");
        assert_eq!(categories[10], "Product");
    }

    #[test]
    fn undo_reports_each_outcome_with_a_terminal_safe_path() {
        assert_eq!(
            undo_message(&UndoResult::Restored("/work/a.txt".into())),
            "Restored /work/a.txt"
        );
        assert_eq!(
            undo_message(&UndoResult::Deleted("/work/new.txt".into())),
            "Deleted /work/new.txt (was newly created)"
        );
        assert_eq!(
            undo_message(&UndoResult::Unavailable("/work/\x1b[2Jx.txt".into())),
            "Could not undo /work/\\x1b[2Jx.txt"
        );
        assert_eq!(
            undo_message(&UndoResult::Restored(PathBuf::from(OsStr::from_bytes(
                b"/work/\xffname"
            )))),
            "Restored /work/\\xffname"
        );
        assert_eq!(undo_message(&UndoResult::Empty), "Nothing to undo.");
    }

    #[test]
    fn unmatched_model_queries_are_used_as_given() {
        let models = vec!["openai/gpt-5".to_owned()];
        assert_eq!(resolve_model_query(&models, "gpt-5"), "openai/gpt-5");
        assert_eq!(resolve_model_query(&models, "zzz"), "zzz");
        assert_eq!(resolve_model_query(&[], "anything"), "anything");
    }
}
