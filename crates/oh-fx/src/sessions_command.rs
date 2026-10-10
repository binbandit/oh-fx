use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use ofx_app::{SessionListSnapshot, session_lookup_message};
use ofx_cli::{
    Command, LaunchModifiers, OutputFormat, SessionListArgs, TopLevelKind, command_failure_json,
};
use ofx_config::ProfilePaths;
use ofx_session::{FxSessions, ListScope, SessionError, SessionStore};

pub(crate) enum Failure {
    Lookup(String),
    Reported { code: String, message: String },
    Fatal(&'static str),
}

impl From<SessionError> for Failure {
    fn from(error: SessionError) -> Self {
        Self::Lookup(error.to_string())
    }
}

pub(crate) struct SavedSessions {
    pub(crate) store: SessionStore,
    pub(crate) fx: FxSessions,
}

pub(crate) fn run(args: &SessionListArgs, modifiers: &LaunchModifiers) -> ExitCode {
    if modifiers.selects_sessions_v2() || crate::cli_ask::sessions_v2_variable().is_some() {
        return crate::unavailable_command(&Command::Sessions(args.clone()));
    }
    crate::auto_upgrade::announce_and_schedule();
    answer(TopLevelKind::Sessions, args.format, list(args))
}

pub(crate) fn answer(
    kind: TopLevelKind,
    format: OutputFormat,
    result: Result<String, Failure>,
) -> ExitCode {
    let failure = match result {
        Ok(text) => return crate::print(text.as_bytes(), crate::command_write_failure(kind)),
        Err(Failure::Lookup(code)) => match session_lookup_message(&code) {
            Some(message) => return report(kind, format, &code, message),
            None => code,
        },
        Err(Failure::Reported { code, message }) => {
            return report(kind, format, &code, &message);
        }
        Err(Failure::Fatal(code)) => code.to_owned(),
    };
    let _ = writeln!(io::stderr(), "oh-fx: {failure}");
    ExitCode::FAILURE
}

fn report(kind: TopLevelKind, format: OutputFormat, code: &str, message: &str) -> ExitCode {
    if matches!(format, OutputFormat::Json) {
        return crate::fail(
            &command_failure_json(kind, message, code),
            crate::command_write_failure(kind),
        );
    }
    let command = if code == "HomeNotSet" {
        kind.token()
    } else {
        "session"
    };
    let _ = writeln!(io::stderr(), "oh-fx {command}: {message}");
    ExitCode::FAILURE
}

pub(crate) fn open_saved_sessions() -> Result<SavedSessions, Failure> {
    let home_not_set = || Failure::Lookup("HomeNotSet".to_owned());
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(home_not_set)?;
    let paths = ProfilePaths::from_environment().ok_or_else(home_not_set)?;
    let workspace_root = env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|_| Failure::Fatal("WorkspaceUnavailable"))?;
    let workspace_root = workspace_root
        .to_str()
        .ok_or(Failure::Fatal("InvalidWorkspaceRoot"))?;
    Ok(SavedSessions {
        store: SessionStore::open_read_only(&paths.data, workspace_root)?,
        fx: FxSessions::open(&home),
    })
}

fn list(args: &SessionListArgs) -> Result<String, Failure> {
    let saved = open_saved_sessions()?;
    let catalog = saved
        .store
        .catalog_with_fx(&saved.fx)
        .map_err(|_| Failure::Fatal("SessionStoreUnavailable"))?;
    let page = catalog.listed_page(args.scope, args.cursor.as_ref(), args.limit);
    let next_cursor = page
        .summaries
        .last()
        .filter(|_| page.has_more)
        .map(|last| format!("v1:{}:{}", last.updated_at_ms, last.id));
    Ok(SessionListSnapshot {
        sessions: &page.summaries,
        has_more: page.has_more,
        next_cursor,
        skipped_invalid: catalog.skipped_invalid(),
        all_workspaces: args.scope == ListScope::AllWorkspaces,
    }
    .render(args.format))
}
