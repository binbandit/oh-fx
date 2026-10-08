use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::github_workflows::Workflow;

const TRIMMED: &[char] = &[' ', '\t', '\r', '\n'];
const TITLE_SPACE: &[char] = &[' ', '\t'];
const BOLD: &str = "**";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("InvalidGithubDraft")]
pub struct InvalidGithubDraft;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub ok: bool,
    pub text: String,
}

pub fn parse_draft(text: &str) -> Result<Draft, InvalidGithubDraft> {
    let trimmed = text.trim_matches(TRIMMED);
    let (first_line, rest) = trimmed.split_once('\n').ok_or(InvalidGithubDraft)?;
    let title = plain_title(first_line);
    if title.is_empty() {
        return Err(InvalidGithubDraft);
    }
    Ok(Draft {
        title: title.to_owned(),
        body: rest.trim_start_matches(TRIMMED).to_owned(),
    })
}

fn plain_title(line: &str) -> &str {
    let mut title = line.trim_matches(TRIMMED);
    let hashes = title.len() - title.trim_start_matches('#').len();
    let after = title[hashes..].chars().next();
    if (1..=6).contains(&hashes) && after.is_none_or(|next| next == ' ' || next == '\t') {
        title = title[hashes..].trim_matches(TITLE_SPACE);
    }
    if title.len() > 2 * BOLD.len()
        && let Some(inner) = title
            .strip_prefix(BOLD)
            .and_then(|inner| inner.strip_suffix(BOLD))
        && !inner.contains(BOLD)
    {
        title = inner.trim_matches(TITLE_SPACE);
    }
    title
}

pub fn publish(workflow: Workflow, draft: &Draft, directory: &Path) -> io::Result<Published> {
    let argv = publish_argv(workflow, &draft.title, &draft.body);
    match Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(directory)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => Ok(published_from_output(&output)),
        Err(error) => published_from_spawn_error(error),
    }
}

fn publish_argv<'a>(workflow: Workflow, title: &'a str, body: &'a str) -> Vec<&'a str> {
    let kind = match workflow {
        Workflow::PullRequest => "pr",
        Workflow::Issue => "issue",
    };
    vec!["gh", kind, "create", "--title", title, "--body", body]
}

fn published_from_spawn_error(error: io::Error) -> io::Result<Published> {
    if error.kind() == io::ErrorKind::NotFound {
        return Ok(Published {
            ok: false,
            text: "gh CLI not found in PATH".to_owned(),
        });
    }
    Err(error)
}

fn published_from_output(output: &Output) -> Published {
    let trimmed = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .trim_matches(TRIMMED)
            .to_owned()
    };
    if !output.status.success() {
        let stderr = trimmed(&output.stderr);
        return Published {
            ok: false,
            text: if stderr.is_empty() {
                "gh command failed".to_owned()
            } else {
                stderr
            },
        };
    }
    let stdout = trimmed(&output.stdout);
    Published {
        ok: true,
        text: if stdout.is_empty() {
            "created successfully".to_owned()
        } else {
            stdout
        },
    }
}

#[cfg(test)]
mod tests;
