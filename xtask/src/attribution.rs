use std::path::Path;

use regex::{Regex, RegexSet};

use crate::workspace_files;

const FORBIDDEN_PATTERNS: &[&str] = &[
    r"(?i)^\s*(co-authored|assisted|generated|written|created)-by\s*:",
    r"(?i)^\s*claude-session\s*:",
    r"(?i)generated (with|by) \[?(claude|copilot|codex|chatgpt|cursor|gemini|an? ai\b)",
    r"(?i)noreply@anthropic\.com",
    r"(?i)claude\.(ai|com)/(code|claude-code)",
    r"(?i)^(author|committer): [^<]*\b(claude|codex|copilot|chatgpt|openai|anthropic|cursor|aider|gemini)\b",
    r"(?i)^(author|committer): (devin|jules)( ai)?( <|\s*$)",
    r"(?i)^(author|committer): .*<[^>]*(anthropic\.com|openai\.com|cursor\.(com|sh)|copilot|devin-ai|aider\.chat)[^>]*>",
    "\u{1F916}",
];

const PULLFROG_TRAILER: &str = r"(?i)^\s*co-authored-by\s*:\s*pullfrog\[bot\]";

pub(crate) fn check_file(path: &Path) -> Result<(), String> {
    check_text(&workspace_files::read(path)?)
}

pub(crate) fn check_text(text: &str) -> Result<(), String> {
    let findings: Vec<String> = attribution_lines(text)
        .into_iter()
        .map(str::to_owned)
        .collect();
    crate::report(
        "agent attribution and co-author trailers are not allowed",
        &findings,
    )
}

fn attribution_lines(text: &str) -> Vec<&str> {
    let forbidden = RegexSet::new(FORBIDDEN_PATTERNS).expect("attribution patterns are valid");
    let pullfrog = Regex::new(PULLFROG_TRAILER).expect("pullfrog trailer pattern is valid");
    text.lines()
        .filter(|line| forbidden.is_match(line) && !pullfrog.is_match(line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_trailers_and_footers() {
        let message = "Add parser\n\nCo-Authored-By: Someone <a@b.c>\nClaude-Session: x\n🤖 Generated with [Claude Code](https://claude.com/claude-code)\n";
        assert_eq!(attribution_lines(message).len(), 3);
    }

    #[test]
    fn accepts_plain_messages() {
        let message = "Reject co-authored-by trailers in commit hook\n\nPlain body text.\n";
        assert!(attribution_lines(message).is_empty());
    }

    #[test]
    fn pull_request_text_is_checked_past_a_scissors_line() {
        let text = "Body\n# ------------------------ >8 ------------------------\nCo-authored-by: Someone <a@b.c>\n";
        assert_eq!(attribution_lines(text).len(), 1);
    }

    #[test]
    fn rejects_agent_author_and_committer_identities() {
        let identities = "Author: Claude <noreply@anthropic.com>\nCommitter: BinBandit <crazywolf132@gmail.com>\n";
        assert_eq!(attribution_lines(identities).len(), 1);
        let human =
            "Author: BinBandit <crazywolf132@gmail.com>\nCommitter: GitHub <noreply@github.com>\n";
        assert!(attribution_lines(human).is_empty());
    }

    #[test]
    fn rejects_markdown_headings_with_agent_footers() {
        assert_eq!(
            attribution_lines("## \u{1F916} Generated with Claude Code\n").len(),
            1
        );
    }

    #[test]
    fn accepts_pullfrog_co_author_trailers() {
        let message = "docs: explain workflow (#1)\n\nCo-authored-by: pullfrog[bot] <226033991+pullfrog[bot]@users.noreply.github.com>\n";
        assert!(attribution_lines(message).is_empty());
    }
}
