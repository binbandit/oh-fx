use std::process::ExitCode;

use ofx_app::{SessionDetailSnapshot, SessionSummarySnapshot};
use ofx_cli::{
    Command, LaunchModifiers, OutputFormat, SessionAction, SessionArgs, SessionTarget, TopLevelKind,
};
use ofx_session::{ListScope, SessionError};

use crate::sessions_command::{Failure, answer, open_saved_sessions};

pub(crate) fn run(args: &SessionArgs, modifiers: &LaunchModifiers) -> ExitCode {
    let selects_v2 =
        modifiers.selects_sessions_v2() || crate::cli_ask::sessions_v2_variable().is_some();
    let SessionAction::Detail(target) = &args.action else {
        return crate::unavailable_command(&Command::Session(args.clone()));
    };
    if selects_v2 {
        return crate::unavailable_command(&Command::Session(args.clone()));
    }
    crate::auto_upgrade::announce_and_schedule();
    let result = match target {
        SessionTarget::Last => latest(args.format),
        SessionTarget::Id(id) => describe(id, args.format),
    };
    answer(TopLevelKind::Session, args.format, result)
}

fn latest(format: OutputFormat) -> Result<String, Failure> {
    let saved = open_saved_sessions()?;
    let catalog = saved
        .store
        .catalog_with_fx(&saved.fx)
        .map_err(|_| SessionError::SessionStoreUnavailable)?;
    let page = catalog.listed_page(ListScope::CurrentWorkspace, None, 1);
    let Some(summary) = page.summaries.first() else {
        return Err(if catalog.skipped_invalid() > 0 {
            SessionError::NoReadableSessions
        } else {
            SessionError::NoSavedSessions
        }
        .into());
    };
    Ok(SessionSummarySnapshot { summary }.render(format))
}

fn describe(id: &str, format: OutputFormat) -> Result<String, Failure> {
    let saved = open_saved_sessions()?;
    let archive = saved
        .store
        .archive_with_fx(id, &saved.fx)
        .map_err(|error| detail_failure(id, error))?;
    Ok(SessionDetailSnapshot { archive: &archive }.render(format))
}

fn detail_failure(id: &str, error: SessionError) -> Failure {
    let message = match error {
        SessionError::InvalidSessionFormat => {
            format!("session {id} is corrupt; run `oh-fx session recover {id}`")
        }
        SessionError::UnsupportedSessionSchema => {
            format!("session {id} uses an unsupported session version")
        }
        other => return other.into(),
    };
    Failure::Reported {
        code: error.to_string(),
        message,
    }
}
