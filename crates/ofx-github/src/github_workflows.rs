use crate::git_context::GitSnapshot;

const TRIMMED: &[char] = &[' ', '\t', '\r', '\n'];
const PULL_REQUEST_OPENING: &str = "Draft a GitHub pull request for the current branch. Reply in the same natural language as the current session. ";
const PULL_REQUEST_CLOSING: &str = "If you need more context, read relevant files. Return only: a plain-text title line without Markdown, a blank line, then a GitHub-flavored Markdown body with sections '## Summary' and '## Testing'. Do not create the PR with gh or publish anything unless I explicitly ask you to.";
const ISSUE_OPENING: &str = "Draft a GitHub issue from the current context. Reply in the same natural language as the current session. ";
const ISSUE_CLOSING: &str = "If you need more context, inspect relevant files, errors, or logs. Return only: a plain-text title line without Markdown, a blank line, then a GitHub-flavored Markdown body with sections '## Summary', '## Steps to Reproduce', '## Expected', and '## Actual'. Do not create the issue with gh or publish anything unless I explicitly ask you to.";
const SNAPSHOT_LEAD: &str = "Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workflow {
    PullRequest,
    Issue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("NotGitRepository")]
pub struct NotGitRepository;

pub fn draft_prompt(
    workflow: Workflow,
    context: &str,
    snapshot: &GitSnapshot,
) -> Result<String, NotGitRepository> {
    let (opening, closing) = match workflow {
        Workflow::PullRequest if !snapshot.in_git_repo => return Err(NotGitRepository),
        Workflow::PullRequest => (PULL_REQUEST_OPENING, PULL_REQUEST_CLOSING),
        Workflow::Issue => (ISSUE_OPENING, ISSUE_CLOSING),
    };
    let context = context.trim_matches(TRIMMED);
    let mut prompt = String::from(opening);
    if !context.is_empty() {
        prompt.push_str("Additional context: ");
        prompt.push_str(context);
        prompt.push_str(". ");
    }
    prompt.push_str(SNAPSHOT_LEAD);
    prompt.push_str(&snapshot.text);
    prompt.push_str("\n\n");
    prompt.push_str(closing);
    Ok(prompt)
}

#[cfg(test)]
mod tests;
