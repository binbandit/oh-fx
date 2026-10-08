use std::path::Path;
use std::process::{Command, Stdio};

const TRIMMED: &[char] = &[' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSnapshot {
    pub in_git_repo: bool,
    pub text: String,
}

pub fn snapshot(directory: &Path) -> GitSnapshot {
    let git = |args: &[&str]| run_git(directory, args);
    let in_git_repo = git(&["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true");
    let branch = git(&["branch", "--show-current"]);
    let status = git(&["status", "--short", "--branch"]);
    let log = git(&["log", "--oneline", "-5"]);
    let staged = git(&["diff", "--stat", "--cached"]);
    let unstaged = git(&["diff", "--stat"]);
    GitSnapshot {
        in_git_repo,
        text: format_snapshot(
            branch.as_deref(),
            status.as_deref(),
            log.as_deref(),
            staged.as_deref(),
            unstaged.as_deref(),
        ),
    }
}

fn git_argv<'a>(args: &[&'a str]) -> Vec<&'a str> {
    ["git", "--no-optional-locks"]
        .into_iter()
        .chain(args.iter().copied())
        .collect()
}

fn run_git(directory: &Path, args: &[&str]) -> Option<String> {
    let command = git_argv(args);
    let output = Command::new(command[0])
        .args(&command[1..])
        .current_dir(directory)
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim_matches(TRIMMED);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn format_snapshot(
    branch: Option<&str>,
    status: Option<&str>,
    log: Option<&str>,
    staged: Option<&str>,
    unstaged: Option<&str>,
) -> String {
    let mut out = String::from("Git snapshot\n");
    out.push_str("Branch: ");
    out.push_str(branch.unwrap_or("unavailable"));
    out.push('\n');
    for (heading, body, fallback) in [
        ("\nStatus:\n", status, "unavailable"),
        ("\nRecent commits:\n", log, "unavailable"),
        ("\nStaged diff stat:\n", staged, "none"),
        ("\nUnstaged diff stat:\n", unstaged, "none"),
    ] {
        out.push_str(heading);
        let body = body.unwrap_or(fallback);
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests;
