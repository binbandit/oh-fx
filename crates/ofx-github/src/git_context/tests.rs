use std::fs;
use std::path::Path;
use std::process::Command;

use super::*;

const NULL_SNAPSHOT: &str = "Git snapshot\nBranch: unavailable\n\nStatus:\nunavailable\n\nRecent commits:\nunavailable\n\nStaged diff stat:\nnone\n\nUnstaged diff stat:\nnone\n";

#[test]
fn snapshot_formatting_null_git_values_match_core_fallback_text_exactly() {
    assert_eq!(format_snapshot(None, None, None, None, None), NULL_SNAPSHOT);
}

#[test]
fn snapshot_formatting_populated_sections_preserve_layout_exactly() {
    assert_eq!(
        format_snapshot(
            Some("main"),
            Some("## main\n M src/main.zig"),
            Some("abc123 first\n"),
            Some(" src/main.zig | 2 ++"),
            Some(" src/core/github/git_context.zig | 5 +++++"),
        ),
        "Git snapshot\nBranch: main\n\nStatus:\n## main\n M src/main.zig\n\nRecent commits:\nabc123 first\n\nStaged diff stat:\n src/main.zig | 2 ++\n\nUnstaged diff stat:\n src/core/github/git_context.zig | 5 +++++\n"
    );
}

#[test]
fn git_argv_read_only_commands_disable_optional_locks() {
    assert_eq!(
        git_argv(&["status", "--short", "--branch"]),
        [
            "git",
            "--no-optional-locks",
            "status",
            "--short",
            "--branch"
        ]
    );
}

#[test]
fn a_directory_outside_any_repository_has_every_section_unavailable() {
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    assert_eq!(
        snapshot(&root),
        GitSnapshot {
            in_git_repo: false,
            text: NULL_SNAPSHOT.to_owned(),
        }
    );
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn a_repository_reports_its_branch_status_commits_and_diff_stats_trimmed() {
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    git(&root, &["init", "-q", "-b", "feature"]);
    fs::write(root.join("staged.txt"), "one\n").unwrap();
    fs::write(root.join("changed.txt"), "one\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "first"]);
    fs::write(root.join("staged.txt"), "two\n").unwrap();
    git(&root, &["add", "staged.txt"]);
    fs::write(root.join("changed.txt"), "two\nthree\n").unwrap();
    let head = git(&root, &["log", "--oneline", "-1"]);
    let state = snapshot(&root);
    assert!(state.in_git_repo);
    assert_eq!(
        state.text,
        format!(
            "Git snapshot\nBranch: feature\n\nStatus:\n## feature\n M changed.txt\nM  staged.txt\n\nRecent commits:\n{head}\nStaged diff stat:\nstaged.txt | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n\nUnstaged diff stat:\nchanged.txt | 3 ++-\n 1 file changed, 2 insertions(+), 1 deletion(-)\n"
        )
    );
}
