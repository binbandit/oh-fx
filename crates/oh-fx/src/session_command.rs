use std::process::ExitCode;

use ofx_app::{
    SessionDetailSnapshot, SessionMigrationSnapshot, SessionRecoverySnapshot,
    SessionSummarySnapshot,
};
use ofx_cli::{
    Command, LaunchModifiers, OutputFormat, SessionAction, SessionArgs, SessionTarget, TopLevelKind,
};
use ofx_session::{ListScope, SessionError, SessionRecoveryStatus};

use crate::sessions_command::{Failure, answer, open_saved_sessions, open_writable_sessions};

const MIGRATION_UNAVAILABLE: &str = "SessionMigrationUnavailable";

pub(crate) fn run(args: &SessionArgs, modifiers: &LaunchModifiers) -> ExitCode {
    let selects_v2 =
        modifiers.selects_sessions_v2() || crate::cli_ask::sessions_v2_variable().is_some();
    crate::auto_upgrade::announce_and_schedule();
    let result = match &args.action {
        SessionAction::Migrate(_) if selects_v2 => {
            Err(Failure::Lookup(MIGRATION_UNAVAILABLE.to_owned()))
        }
        SessionAction::Migrate(id) => migrate(id, args.format),
        SessionAction::Recover(id) if !selects_v2 => return recover(id, args.format),
        SessionAction::Detail(SessionTarget::Last) if !selects_v2 => latest(args.format),
        SessionAction::Detail(SessionTarget::Id(id)) if !selects_v2 => describe(id, args.format),
        _ => return crate::not_available(&Command::Session(args.clone())),
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

fn migrate(id: &str, format: OutputFormat) -> Result<String, Failure> {
    let migration = open_writable_sessions()?.migrate(id)?;
    Ok(SessionMigrationSnapshot {
        migration: &migration,
    }
    .render(format))
}

fn recover(id: &str, format: OutputFormat) -> ExitCode {
    let recovery =
        match open_writable_sessions().and_then(|store| store.recover(id).map_err(Failure::from)) {
            Ok(recovery) => recovery,
            Err(failure) => return answer(TopLevelKind::Session, format, Err(failure)),
        };
    let text = SessionRecoverySnapshot {
        recovery: &recovery,
    }
    .render(format);
    let write_failure = crate::command_write_failure(TopLevelKind::Session);
    if recovery.status == SessionRecoveryStatus::Recovered {
        crate::print(text.as_bytes(), write_failure)
    } else {
        crate::fail(&text, write_failure)
    }
}
