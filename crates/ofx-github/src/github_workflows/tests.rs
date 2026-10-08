use super::*;

fn snapshot(in_git_repo: bool, text: &str) -> GitSnapshot {
    GitSnapshot {
        in_git_repo,
        text: text.to_owned(),
    }
}

const FEATURE: &str = "Git snapshot\nBranch: feature\n";

#[test]
fn pull_request_prompt_preserves_active_context_and_section_contract() {
    assert_eq!(
        draft_prompt(
            Workflow::PullRequest,
            " ready for review \n",
            &snapshot(true, FEATURE)
        ),
        Ok("Draft a GitHub pull request for the current branch. Reply in the same natural language as the current session. Additional context: ready for review. Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\nGit snapshot\nBranch: feature\n\n\nIf you need more context, read relevant files. Return only: a plain-text title line without Markdown, a blank line, then a GitHub-flavored Markdown body with sections '## Summary' and '## Testing'. Do not create the PR with gh or publish anything unless I explicitly ask you to.".to_owned())
    );
    assert_eq!(
        draft_prompt(Workflow::PullRequest, "", &snapshot(true, FEATURE)),
        Ok("Draft a GitHub pull request for the current branch. Reply in the same natural language as the current session. Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\nGit snapshot\nBranch: feature\n\n\nIf you need more context, read relevant files. Return only: a plain-text title line without Markdown, a blank line, then a GitHub-flavored Markdown body with sections '## Summary' and '## Testing'. Do not create the PR with gh or publish anything unless I explicitly ask you to.".to_owned())
    );
}

#[test]
fn pull_request_prompt_requires_a_git_repository() {
    assert_eq!(
        draft_prompt(
            Workflow::PullRequest,
            "",
            &snapshot(false, "Git snapshot\nBranch: unavailable\n")
        ),
        Err(NotGitRepository)
    );
}

#[test]
fn issue_prompt_works_outside_git_and_omits_empty_context_clause() {
    let unavailable = snapshot(false, "Git snapshot\nBranch: unavailable\n");
    assert_eq!(
        draft_prompt(Workflow::Issue, " \t\r\n", &unavailable),
        Ok("Draft a GitHub issue from the current context. Reply in the same natural language as the current session. Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\nGit snapshot\nBranch: unavailable\n\n\nIf you need more context, inspect relevant files, errors, or logs. Return only: a plain-text title line without Markdown, a blank line, then a GitHub-flavored Markdown body with sections '## Summary', '## Steps to Reproduce', '## Expected', and '## Actual'. Do not create the issue with gh or publish anything unless I explicitly ask you to.".to_owned())
    );
    assert_eq!(
        draft_prompt(Workflow::Issue, "flaky login", &unavailable),
        Ok("Draft a GitHub issue from the current context. Reply in the same natural language as the current session. Additional context: flaky login. Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\nGit snapshot\nBranch: unavailable\n\n\nIf you need more context, inspect relevant files, errors, or logs. Return only: a plain-text title line without Markdown, a blank line, then a GitHub-flavored Markdown body with sections '## Summary', '## Steps to Reproduce', '## Expected', and '## Actual'. Do not create the issue with gh or publish anything unless I explicitly ask you to.".to_owned())
    );
}

#[test]
fn every_draft_prompt_asks_for_a_plain_text_title_line() {
    for workflow in [Workflow::PullRequest, Workflow::Issue] {
        for context in ["", "ready for review"] {
            let prompt = draft_prompt(workflow, context, &snapshot(true, FEATURE)).unwrap();
            assert!(prompt.contains(
                "Return only: a plain-text title line without Markdown, a blank line, then"
            ));
        }
    }
}
