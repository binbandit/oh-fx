use std::process::ExitCode;

use ofx_app::SessionSummarySnapshot;
use ofx_cli::{
    Command, LaunchModifiers, OutputFormat, SessionAction, SessionArgs, SessionTarget, TopLevelKind,
};
use ofx_session::{ListScope, SessionError};

use crate::sessions_command::{Failure, answer, open_saved_sessions};

pub(crate) fn run(args: &SessionArgs, modifiers: &LaunchModifiers) -> ExitCode {
    let selects_v2 =
        modifiers.selects_sessions_v2() || crate::cli_ask::sessions_v2_variable().is_some();
    if selects_v2 || args.action != SessionAction::Detail(SessionTarget::Last) {
        return crate::unavailable_command(&Command::Session(args.clone()));
    }
    crate::auto_upgrade::announce_and_schedule();
    answer(TopLevelKind::Session, args.format, latest(args.format))
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
