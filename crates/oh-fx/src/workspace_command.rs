use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use ofx_app::{WorkspaceSnapshot, workspace_error_message};
use ofx_cli::{OutputFormat, TopLevelKind, WorkspaceAction, WorkspaceArgs, command_failure_json};
use ofx_config::{ProfilePaths, Settings};
use ofx_workspace::{
    Action, Mutation, Outcome, Reconciliation, WorkspaceAccess, WorkspaceAccessError, execute,
};

const INDETERMINATE_CODE: &str = "SettingsCommitIndeterminate";

pub(crate) fn run(args: &WorkspaceArgs) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    match answer(args) {
        Ok(text) => crate::print(
            text.as_bytes(),
            crate::command_write_failure(TopLevelKind::Workspace),
        ),
        Err((message, code)) => match args.format {
            OutputFormat::Text => {
                let _ = writeln!(io::stderr(), "oh-fx workspace: {message}");
                ExitCode::FAILURE
            }
            OutputFormat::Json => crate::fail(
                &command_failure_json(TopLevelKind::Workspace, message, &code),
                crate::command_write_failure(TopLevelKind::Workspace),
            ),
        },
    }
}

type Failure = (&'static str, String);

fn failed(code: &dyn ToString) -> Failure {
    let code = code.to_string();
    (workspace_error_message(&code), code)
}

fn answer(args: &WorkspaceArgs) -> Result<String, Failure> {
    let primary = env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|_| failed(&"WorkspaceUnavailable"))?;
    let paths = ProfilePaths::from_environment();
    let mut settings = match &paths {
        Some(paths) => Settings::load(paths, &primary).map_err(|error| failed(&error))?,
        None => Settings::default(),
    };
    let current =
        WorkspaceAccess::new(&primary, settings.additional_directories()).unwrap_or_else(|_| {
            settings.reject_additional_directories();
            WorkspaceAccess::primary_only(&primary)
        });
    if settings.profile_is_unusable() {
        return Err(failed(&"InvalidProfileConfiguration"));
    }
    let mut stderr = io::stderr().lock();
    for diagnostic in settings.diagnostics() {
        let _ = writeln!(stderr, "oh-fx: {diagnostic}");
    }
    drop(stderr);
    let Some(action) = &args.action else {
        return Ok(snapshot(&primary, &current, None, args.format));
    };
    let saving = paths.as_ref().filter(|_| env::var_os("HOME").is_some());
    match execute(saving, &current, &action_of(action)?).map_err(|error| failed(&error))? {
        Outcome::Updated { access, mutation } => {
            Ok(snapshot(&primary, &access, Some(&mutation), args.format))
        }
        Outcome::Indeterminate(reconciliation) => Err((
            indeterminate_message(&reconciliation),
            INDETERMINATE_CODE.to_owned(),
        )),
    }
}

fn action_of(action: &WorkspaceAction) -> Result<Action, Failure> {
    let text = |path: &std::ffi::OsString| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| failed(&WorkspaceAccessError::InvalidPath))
    };
    Ok(match action {
        WorkspaceAction::Add(path) => Action::Add(text(path)?),
        WorkspaceAction::Remove(path) => Action::Remove(text(path)?),
        WorkspaceAction::Clear => Action::Clear,
    })
}

fn snapshot(
    primary: &Path,
    access: &WorkspaceAccess,
    mutation: Option<&Mutation>,
    format: OutputFormat,
) -> String {
    WorkspaceSnapshot {
        primary_directory: primary,
        saved_suppressed: access.saved_suppressed(),
        additional_directories: access.entries(),
        mutation,
    }
    .render(format)
}

fn indeterminate_message(reconciliation: &Reconciliation) -> &'static str {
    match reconciliation {
        Reconciliation::Intended(_) => {
            "settings durability is uncertain; reloaded settings match the requested update"
        }
        Reconciliation::Previous(_) => {
            "settings durability is uncertain; reloaded settings match the previous state, so the update was not applied"
        }
        Reconciliation::Unconfirmed => {
            "settings durability is uncertain; reloaded settings match neither the requested nor previous state"
        }
    }
}
