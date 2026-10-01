use std::process::Command;

use crate::{comments, workspace_files};

const FORMAT: &[&str] = &["fmt", "--all", "--check"];
const CLIPPY: &[&str] = &[
    "clippy",
    "--workspace",
    "--all-targets",
    "--locked",
    "--",
    "-D",
    "warnings",
];
const TEST: &[&str] = &["test", "--workspace", "--locked"];
const GIT_REPOSITORY_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

pub(crate) fn style() -> Result<(), String> {
    workspace_files::enter_repository_root()?;
    cargo(FORMAT)?;
    comments::check_workspace()
}

pub(crate) fn lint() -> Result<(), String> {
    style()?;
    cargo(CLIPPY)
}

pub(crate) fn test() -> Result<(), String> {
    cargo(TEST)
}

pub(crate) fn install_hooks() -> Result<(), String> {
    workspace_files::git(&["config", "core.hooksPath", ".githooks"])?;
    println!("git hooks now run from .githooks");
    Ok(())
}

fn cargo(args: &[&str]) -> Result<(), String> {
    let mut command = Command::new(env!("CARGO"));
    command.args(args);
    for variable in GIT_REPOSITORY_VARIABLES {
        command.env_remove(variable);
    }
    let status = command
        .status()
        .map_err(|error| format!("failed to run cargo: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {} failed", args.join(" ")))
    }
}
