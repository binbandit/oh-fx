use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, RecordedRequest, Reply, chat_text_events};
use serde_json::{Value, json};

const KEY: (&str, &str) = ("PORTKEY_API_KEY", "pk-test-0123456789");
const NULL_SNAPSHOT: &str = "Git snapshot\nBranch: unavailable\n\nStatus:\nunavailable\n\nRecent commits:\nunavailable\n\nStaged diff stat:\nnone\n\nUnstaged diff stat:\nnone\n";
const ISSUE_CLOSING: &str = "If you need more context, inspect relevant files, errors, or logs. Return only: a plain-text title line without Markdown, a blank line, then a GitHub-flavored Markdown body with sections '## Summary', '## Steps to Reproduce', '## Expected', and '## Actual'. Do not create the issue with gh or publish anything unless I explicitly ask you to.";
const PULL_REQUEST_OPENING: &str = "Draft a GitHub pull request for the current branch. Reply in the same natural language as the current session. ";
const ADDITIONAL_DIRECTORIES_CONTEXT: &str = "Runtime context: the following additional directories are access-authorized for this run. Relative paths still resolve from the primary workspace. These directories do not contribute AGENTS.md or other project instructions.\n";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Home {
    fn new(server: &FakeServer) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        let workspace = root.join("workspace");
        fs::create_dir_all(root.join("config/oh-fx")).expect("create the config directory");
        fs::create_dir_all(&workspace).expect("create the workspace");
        let settings = json!({
            "provider": "portkey",
            "model": "@openai/gpt-4o",
            "providers": {
                "portkey": {
                    "protocol": "openai-chat-completions",
                    "base_url": server.base_url(),
                    "auth": {"type": "none"},
                    "headers": {"x-portkey-api-key": "${PORTKEY_API_KEY}"},
                    "models": ["@openai/gpt-4o"]
                }
            }
        });
        fs::write(
            root.join("config/oh-fx/settings.json"),
            settings.to_string(),
        )
        .expect("write settings.json");
        Self {
            _directory: directory,
            root,
            workspace,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env(KEY.0, KEY.1)
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn messages(request: &RecordedRequest) -> Vec<Value> {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .clone()
}

fn user_prompt(request: &RecordedRequest) -> String {
    let messages = messages(request);
    let user: Vec<&Value> = messages
        .iter()
        .filter(|message| message["role"] == "user")
        .collect();
    assert_eq!(user.len(), 1, "{messages:#?}");
    user[0]["content"].as_str().expect("text").to_owned()
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
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
        .status()
        .expect("run git");
    assert!(status.success(), "{args:?}");
}

#[test]
fn an_issue_is_drafted_outside_git_from_upstreams_prompt_and_printed_raw() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&[
        "Login fails\n\n## Summary\nIt **fails**.",
    ]))]);
    let home = Home::new(&server);
    let output = home.run(&["issue", "flaky", "login"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Login fails\n\n## Summary\nIt **fails**.");
    assert_eq!(
        user_prompt(&server.requests()[0]),
        format!(
            "Draft a GitHub issue from the current context. Reply in the same natural language as the current session. Additional context: flaky login. Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\n{NULL_SNAPSHOT}\n\n{ISSUE_CLOSING}"
        )
    );
}

#[test]
fn a_pull_request_needs_a_git_repository_before_any_request() {
    let server = FakeServer::start([]);
    let home = Home::new(&server);
    let output = home.run(&["pr"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "oh-fx pr: requires running inside a git repository\n"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn a_pull_request_draft_carries_the_branch_snapshot_and_auto_selects_auto_mode() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Title\n\nBody"]))]);
    let home = Home::new(&server);
    git(&home.workspace, &["init", "-q", "-b", "feature"]);
    fs::write(home.workspace.join("notes.txt"), "one\n").unwrap();
    git(&home.workspace, &["add", "."]);
    git(&home.workspace, &["commit", "-q", "-m", "first"]);
    let output = home.run(&["pr", "--auto", "ready"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Title\n\nBody");
    let request = &server.requests()[0];
    let prompt = user_prompt(request);
    assert!(
        prompt.starts_with(&format!(
            "{PULL_REQUEST_OPENING}Additional context: ready. Use this prepared git snapshot first"
        )),
        "{prompt}"
    );
    assert!(
        prompt.contains("Git snapshot\nBranch: feature\n\nStatus:\n## feature\n"),
        "{prompt}"
    );
    assert!(messages(request).iter().any(|message| {
        message["role"] == "system"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.starts_with("Runtime context: permission mode is auto."))
    }),);
}

#[test]
fn launch_modifiers_the_drafts_cannot_honor_yet_fail_before_any_request() {
    let server = FakeServer::start([]);
    let home = Home::new(&server);
    for (args, feature) in [
        (&["--sessions-v2", "issue"][..], "--sessions-v2"),
        (
            &["--context-limit", "mcp_description_bytes=1", "issue"],
            "--context-limit",
        ),
        (&["issue", "--create"], "issue --create"),
        (&["pr", "--auto", "--create"], "pr --create"),
    ] {
        let output = home.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(
            stderr(&output),
            format!("oh-fx: {feature} is not available yet\n"),
            "{args:?}"
        );
    }
    assert!(server.requests().is_empty());
}

#[test]
fn an_added_directory_reaches_the_draft_as_it_reaches_ask() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Title\n\nBody"]))]);
    let home = Home::new(&server);
    let shared = home.root.join("shared");
    fs::create_dir_all(&shared).unwrap();
    let output = home.run(&["--add-dir", shared.to_str().unwrap(), "issue"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let granted = format!("{ADDITIONAL_DIRECTORIES_CONTEXT}- {}\n", shared.display());
    assert!(
        messages(&server.requests()[0])
            .iter()
            .any(|message| message["role"] == "system" && message["content"] == granted.as_str()),
        "{:?}",
        messages(&server.requests()[0])
    );
}
