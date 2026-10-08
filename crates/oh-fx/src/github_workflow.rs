use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use ofx_cli::{LaunchModifiers, WorkflowArgs};
use ofx_github::{Workflow, draft_prompt, snapshot};

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
    let unavailable = cli_ask::unavailable_launch(modifiers)
        .map(str::to_owned)
        .or_else(|| args.create.then(|| format!("{command} --create")));
    if let Some(feature) = unavailable {
        return crate::unavailable(&feature);
    }
    let snapshot = snapshot(Path::new("."));
    let Ok(prompt) = draft_prompt(workflow, &args.context.to_string_lossy(), &snapshot) else {
        let _ = writeln!(
            io::stderr(),
            "oh-fx {command}: requires running inside a git repository"
        );
        return ExitCode::FAILURE;
    };
    cli_ask::run_prompt(&args.ask_args(prompt.clone()), &prompt, modifiers)
}
