use ofx_contract::{NoticeTone, UiEvent};
use ofx_workspace::{Action, FailurePhase, Mutation, Outcome, Reconciliation};

use crate::app_agent_runtime::ControllerState;
use crate::app_commands::Work;
use crate::output_contracts::workspace::{WorkspaceSnapshot, workspace_error_message};

const TOPIC: &str = "workspace";
const USAGE: &str = "usage: /workspace [add PATH|remove PATH|clear]";
const BUSY: &str = "Workspace changes are unavailable until the active and queued work finishes.";
const UNCERTAIN_INTENDED: &str =
    "Workspace settings durability is uncertain; reloaded settings match the requested update.";
const UNCERTAIN_PREVIOUS: &str = "Workspace settings durability is uncertain; reloaded settings match the previous state, so the update is not active.";
const UNCERTAIN_UNCONFIRMED: &str = "Workspace settings durability is uncertain; reloaded settings match neither the requested nor previous state, so runtime access is unchanged.";

pub(crate) fn handle_workspace(state: &mut ControllerState, payload: &str, work: Work) {
    if trimmed(payload).is_empty() {
        if refresh_for_listing(state, work) {
            let menu = state.workspace().menu();
            state.emit(UiEvent::WorkspaceMenuOpened { menu });
        }
        return;
    }
    let action = match parse(payload) {
        Err(()) => return state.notice(NoticeTone::Error, "", USAGE),
        Ok(None) => {
            if refresh_for_listing(state, work) {
                show_snapshot(state, None);
            }
            return;
        }
        Ok(Some(action)) => action,
    };
    if work != Work::Idle {
        return state.notice(NoticeTone::Neutral, TOPIC, BUSY);
    }
    match state.workspace().execute(&action) {
        Err(failure) => {
            let prefix = match failure.phase {
                FailurePhase::Stage => "Workspace update rejected",
                FailurePhase::Commit => "Workspace settings were not changed",
                FailurePhase::Reconcile => {
                    "Workspace settings are uncertain and could not be reloaded"
                }
            };
            let reason = workspace_error_message(&failure.to_string());
            state.notice(NoticeTone::Error, TOPIC, &format!("{prefix}: {reason}"));
        }
        Ok(Outcome::Updated { access, mutation }) => {
            state.workspace_mut().install(access);
            show_snapshot(state, Some(&mutation));
        }
        Ok(Outcome::Indeterminate(reconciliation)) => {
            let message = match reconciliation {
                Reconciliation::Intended(access) => {
                    state.workspace_mut().install(access);
                    UNCERTAIN_INTENDED
                }
                Reconciliation::Previous(access) => {
                    state.workspace_mut().install(access);
                    UNCERTAIN_PREVIOUS
                }
                Reconciliation::Unconfirmed => UNCERTAIN_UNCONFIRMED,
            };
            state.notice(NoticeTone::Warning, TOPIC, message);
        }
    }
}

fn refresh_for_listing(state: &mut ControllerState, work: Work) -> bool {
    if work != Work::Idle {
        return true;
    }
    match state.workspace_mut().refresh_availability() {
        Ok(()) => true,
        Err(error) => {
            let reason = workspace_error_message(&error.to_string());
            state.notice(
                NoticeTone::Error,
                TOPIC,
                &format!("Workspace refresh rejected: {reason}"),
            );
            false
        }
    }
}

fn show_snapshot(state: &ControllerState, mutation: Option<&Mutation>) {
    let access = state.workspace().access();
    let body = WorkspaceSnapshot {
        primary_directory: access.primary(),
        saved_suppressed: access.saved_suppressed(),
        additional_directories: access.entries(),
        mutation,
    }
    .render_interactive_body();
    state.notice(NoticeTone::Neutral, TOPIC, &body);
}

fn trimmed(text: &str) -> &str {
    text.trim_matches([' ', '\t'])
}

fn parse(payload: &str) -> Result<Option<Action>, ()> {
    let payload = trimmed(payload);
    if payload.is_empty() || payload == "list" {
        return Ok(None);
    }
    if payload == "clear" {
        return Ok(Some(Action::Clear));
    }
    if let Some(path) = argument(payload, "add") {
        return Ok(Some(Action::Add(path.to_owned())));
    }
    if let Some(path) = argument(payload, "remove") {
        return Ok(Some(Action::Remove(path.to_owned())));
    }
    Err(())
}

fn argument<'a>(payload: &'a str, verb: &str) -> Option<&'a str> {
    let rest = payload.strip_prefix(verb)?;
    let separator = *rest.as_bytes().first()?;
    if !matches!(separator, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        return None;
    }
    Some(trimmed(rest)).filter(|path| !path.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_as_upstreams_do() {
        for (payload, parsed) in [
            ("", Ok(None)),
            (" list\t", Ok(None)),
            ("clear", Ok(Some(Action::Clear))),
            ("add  ../a b ", Ok(Some(Action::Add("../a b".to_owned())))),
            ("remove\t/x", Ok(Some(Action::Remove("/x".to_owned())))),
            ("add\u{b}/x", Ok(Some(Action::Add("\u{b}/x".to_owned())))),
            ("add", Err(())),
            ("add \t", Err(())),
            ("addx", Err(())),
            ("removed /x", Err(())),
            ("clear all", Err(())),
            ("list all", Err(())),
        ] {
            assert_eq!(parse(payload), parsed, "{payload:?}");
        }
    }
}
