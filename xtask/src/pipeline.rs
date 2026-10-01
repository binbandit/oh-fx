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
    let status = Command::new(env!("CARGO"))
        .args(args)
        .status()
        .map_err(|error| format!("failed to run cargo: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {} failed", args.join(" ")))
    }
}
