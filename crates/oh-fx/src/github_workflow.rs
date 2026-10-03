use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use ofx_cli::{LaunchModifiers, WorkflowArgs};
use ofx_github::{Workflow, draft_prompt, parse_draft, publish, snapshot};

use crate::cli_ask;

pub(crate) fn run(
    workflow: Workflow,
    args: &WorkflowArgs,
    modifiers: &LaunchModifiers,
) -> ExitCode {
    let command = match workflow {
        Workflow::PullRequest => "pr",
        Workflow::Issue => "issue",
    };
    if let Some(feature) = cli_ask::unavailable_launch(modifiers) {
        return crate::unavailable(feature);
    }
    let directory = Path::new(".");
    let snapshot = snapshot(directory);
    let Ok(prompt) = draft_prompt(workflow, &args.context.to_string_lossy(), &snapshot) else {
        return fail(command, "requires running inside a git repository");
    };
    let ask = args.ask_args(prompt.clone());
    if !args.create {
        return cli_ask::run_prompt(&ask, &prompt, modifiers);
    }
    let reply = match cli_ask::capture_prompt(&ask, &prompt, modifiers) {
        Ok(reply) => reply,
        Err(exit) => return exit,
    };
    let Ok(draft) = parse_draft(&reply) else {
        return fail(
            command,
            match workflow {
                Workflow::PullRequest => "failed to parse drafted PR title/body",
                Workflow::Issue => "failed to parse drafted issue title/body",
            },
        );
    };
    match publish(workflow, &draft, directory) {
        Ok(published) if published.ok => {
            match crate::write_stdout(&format!("{}\n", published.text)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => error_name(cli_ask::write_error_name(&error)),
            }
        }
        Ok(published) => fail(command, &published.text),
        Err(error) => error_name(spawn_error_name(&error)),
    }
}

fn spawn_error_name(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "AccessDenied",
        io::ErrorKind::ExecutableFileBusy => "FileBusy",
        io::ErrorKind::OutOfMemory => "SystemResources",
        _ => "Unexpected",
    }
}

fn error_name(name: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "oh-fx: {name}");
    ExitCode::FAILURE
}

fn fail(command: &str, message: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "oh-fx {command}: {message}");
    ExitCode::FAILURE
}
