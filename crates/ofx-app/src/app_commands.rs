use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_cli::{SLASH_REGISTRY, SlashKind, SlashPresentationCategory};
use ofx_contract::{CompactionActivity, CompactionEnd, Notice, NoticeTone, UiEvent};
use ofx_session::resolve_model_query_from_ids;
use ofx_text::encode_terminal_safe;
use ofx_tui::SlashCommandSpec;
use ofx_workspace::{ChangeTracker, MAX_PATH_BYTES, UndoResult};

use crate::app_agent_runtime::ControllerState;
use crate::app_session_runtime::{Persistence, RenameError, validate_session_title};
use crate::session_commands::handle_allowlist;
use crate::skill_commands::handle_skills;

const UNKNOWN_COMMAND: &str = "Unknown command. Try /help.";
const CLIPBOARD_TOPIC: &str = "clipboard";
const NO_REPLY_TO_COPY: &str = "No assistant reply to copy.";
const COPIED: &str = "Copied to clipboard.";
const COPY_FAILED: &str = "Failed to copy to clipboard.";
const FAST_TOPIC: &str = "fast";
const NO_FAST_MODE: &str = "This model does not come with a fast mode.";
const UNDO_TOPIC: &str = "undo";
const USAGE_TOPIC: &str = "usage";
const PROFILE_USAGE_UNAVAILABLE: &str =
    "Durable profile usage is unavailable in this host; active session usage remains in memory.";
const NOTHING_TO_UNDO: &str = "Nothing to undo.";
const RESUME_DURING_TURN: &str = "resume is unavailable until the response finishes";
const SESSION_TOPIC: &str = "session";
const RENAME_USAGE: &str = "usage: /rename <title>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandEffect {
    None,
    SwitchModel(String),
    Clear,
    ToggleFast,
    Compact,
    OpenSessions,
    Rename(String),
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

pub(crate) fn handle_command(state: &ControllerState, text: &str, work: Work) -> CommandEffect {
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
        SlashKind::ClearScreen | SlashKind::NewSession | SlashKind::ResetSession => {
            CommandEffect::Clear
        }
        SlashKind::ResumeSession if work != Work::Idle => {
            refuse_resume_during_turn(state);
            CommandEffect::None
        }
        SlashKind::ResumeSession => CommandEffect::OpenSessions,
        SlashKind::RenameSession => CommandEffect::Rename(command.payload.to_owned()),
        SlashKind::Usage => {
            state.notice(NoticeTone::Neutral, USAGE_TOPIC, PROFILE_USAGE_UNAVAILABLE);
            CommandEffect::None
        }
        SlashKind::Status => {
            state.notice(NoticeTone::Neutral, "status", &state.status_body());
            CommandEffect::None
        }
        SlashKind::Stats => {
            state.emit(UiEvent::StatsRequested);
            CommandEffect::None
        }
        SlashKind::Fast => CommandEffect::ToggleFast,
        SlashKind::Compact => match work {
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
        },
        SlashKind::Allowlist => {
            state.emit(UiEvent::Notice {
                notice: handle_allowlist(&state.settings_access(), command.payload),
            });
            CommandEffect::None
        }
        SlashKind::Undo => {
            let result = state
                .change_tracker()
                .map_or(UndoResult::Empty, ChangeTracker::undo_last);
            state.notice(NoticeTone::Neutral, UNDO_TOPIC, &undo_message(&result));
            CommandEffect::None
        }
        SlashKind::Copy => {
            copy_last_reply(state);
            CommandEffect::None
        }
        SlashKind::Version => {
            state.notice(NoticeTone::Neutral, "version", ofx_upgrade::VERSION);
            CommandEffect::None
        }
        SlashKind::Model if command.payload.is_empty() => {
            state.notice(NoticeTone::Neutral, "model", &model_status(state));
            CommandEffect::None
        }
        SlashKind::Permissions => {
            state.permissions().handle_command(command.payload);
            CommandEffect::None
        }
        SlashKind::Skills => {
            handle_skills(state, command.payload);
            CommandEffect::None
        }
        SlashKind::Model => {
            let resolved = resolve_model_query(state.models(), command.payload);
            let prefix = if work == Work::Turn {
                "Next turn will use "
            } else {
                "Switched to "
            };
            state.notice(NoticeTone::Neutral, "", &format!("{prefix}{resolved}"));
            CommandEffect::SwitchModel(resolved)
        }
    }
}

pub(crate) async fn toggle_fast(state: &mut ControllerState) -> bool {
    if state.fast_mode() {
        state.set_fast_mode(false);
        state.save_model_preference(FAST_TOPIC);
        state.notice(NoticeTone::Neutral, FAST_TOPIC, "off");
        return true;
    }
    if !state.supports_fast_mode().await {
        state.notice(NoticeTone::Neutral, FAST_TOPIC, NO_FAST_MODE);
        return false;
    }
    state.set_fast_mode(true);
    state.save_model_preference(FAST_TOPIC);
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
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn the_shell_lists_the_registry_commands_with_their_aliases() {
        let specs = slash_command_specs();
        let commands: Vec<&str> = specs.iter().map(|spec| spec.command.as_str()).collect();
        assert_eq!(
            commands,
            [
                "/help",
                "/clear",
                "/new",
                "/reset",
                "/resume",
                "/rename",
                "/stats",
                "/usage",
                "/status",
                "/model",
                "/permissions",
                "/allowlist",
                "/undo",
                "/skills",
                "/copy",
                "/compact",
                "/fast",
                "/version",
                "/quit",
            ]
        );
        let compacting: Vec<&str> = specs
            .iter()
            .filter(|spec| spec.compacts)
            .map(|spec| spec.command.as_str())
            .collect();
        assert_eq!(compacting, ["/compact"]);
        assert_eq!(specs[2].description, "start a fresh session");
        assert_eq!(specs[4].description, "resume a saved session");
        assert_eq!(specs[5].description, "rename the current session");
        assert_eq!(specs[13].description, "browse and manage skills");
        assert_eq!(specs[18].aliases, ["/exit"]);
        assert_eq!(specs[18].description, "exit the interactive shell");
    }

    #[test]
    fn the_shell_groups_commands_by_upstreams_presentation_categories() {
        let categories = slash_command_categories();
        let specs = slash_command_specs();
        let grouped: Vec<(&str, &str)> = specs
            .iter()
            .map(|spec| (spec.command.as_str(), categories[spec.category].as_str()))
            .collect();
        assert_eq!(
            grouped,
            [
                ("/help", "General"),
                ("/clear", "General"),
                ("/new", "Session"),
                ("/reset", "Session"),
                ("/resume", "Session"),
                ("/rename", "Session"),
                ("/stats", "Account"),
                ("/usage", "Account"),
                ("/status", "General"),
                ("/model", "Model"),
                ("/permissions", "Security"),
                ("/allowlist", "Security"),
                ("/undo", "Session"),
                ("/skills", "Extensions"),
                ("/copy", "Session"),
                ("/compact", "Session"),
                ("/fast", "Model"),
                ("/version", "General"),
                ("/quit", "General"),
            ]
        );
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
