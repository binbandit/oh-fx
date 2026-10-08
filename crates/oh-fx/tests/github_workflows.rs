use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
        self.run_with(args, "/usr/bin:/bin", &[])
    }

    fn fake_gh(&self) -> String {
        let bin = self.root.join("bin");
        fs::create_dir_all(&bin).expect("create the bin directory");
        let gh = bin.join("gh");
        fs::write(
            &gh,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$GH_ARGS_FILE\"\nwhile [ -n \"$GH_WAIT_FILE\" ] && [ ! -e \"$GH_WAIT_FILE\" ]; do sleep 0.05; done\nprintf '%s' \"$GH_STDOUT\"\nprintf '%s' \"$GH_STDERR\" >&2\nexit \"${GH_EXIT:-0}\"\n",
        )
        .expect("write the fake gh");
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("make gh runnable");
        format!("{}:/usr/bin:/bin", bin.display())
    }

    fn gh_args(&self) -> Option<Vec<String>> {
        let recorded = fs::read(self.root.join("gh-args")).ok()?;
        Some(
            recorded
                .split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8(arg.to_vec()).expect("UTF-8 arguments"))
                .collect(),
        )
    }

    fn run_with(&self, args: &[&str], path: &str, environment: &[(&str, &str)]) -> Output {
        self.command(args, path, environment)
            .output()
            .expect("run oh-fx")
    }

    fn command(&self, args: &[&str], path: &str, environment: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", path)
            .env("GH_ARGS_FILE", self.root.join("gh-args"))
            .envs(environment.iter().copied())
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env(KEY.0, KEY.1)
            .stdin(Stdio::null());
        command
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

fn text_then_tool_call(text: &str, call_id: &str, name: &str, arguments: &str) -> Vec<String> {
    let chunk = |delta: Value, finish_reason: Value| {
        json!({
            "id": "chatcmpl-draft",
            "object": "chat.completion.chunk",
            "model": "testkit-model",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
        .to_string()
    };
    vec![
        chunk(json!({"content": text}), Value::Null),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": call_id, "type": "function", "function": {"name": name, "arguments": arguments}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        "[DONE]".to_owned(),
    ]
}

#[test]
fn create_publishes_the_drafted_issue_with_gh_and_prints_its_trimmed_answer() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&[
        "Login fails\n\n## Summary\nIt **fails** in `auth`.",
    ]))]);
    let home = Home::new(&server);
    let path = home.fake_gh();
    let output = home.run_with(
        &["issue", "--create", "flaky"],
        &path,
        &[("GH_STDOUT", " \nhttps://github.com/o/r/issues/2\n ")],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "https://github.com/o/r/issues/2\n");
    assert_eq!(stderr(&output), "");
    assert_eq!(
        home.gh_args().expect("gh ran"),
        [
            "issue",
            "create",
            "--title",
            "Login fails",
            "--body",
            "## Summary\nIt **fails** in `auth`."
        ]
    );
}

#[test]
fn gh_failures_and_empty_answers_are_reported_as_upstream_reports_them() {
    for (environment, expected_stdout, expected_stderr, code) in [
        (
            &[("GH_EXIT", "1"), ("GH_STDERR", " \nfailed to create\n ")][..],
            "",
            "oh-fx issue: failed to create\n",
            1,
        ),
        (
            &[("GH_EXIT", "1")],
            "",
            "oh-fx issue: gh command failed\n",
            1,
        ),
        (&[("GH_STDOUT", " \n\t ")], "created successfully\n", "", 0),
    ] {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["Title\n\nBody"]))]);
        let home = Home::new(&server);
        let path = home.fake_gh();
        let output = home.run_with(&["issue", "--create"], &path, environment);
        assert_eq!(output.status.code(), Some(code), "{environment:?}");
        assert_eq!(stdout(&output), expected_stdout, "{environment:?}");
        assert_eq!(stderr(&output), expected_stderr, "{environment:?}");
    }
}

#[test]
fn a_missing_gh_is_reported_after_the_draft() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Title\n\nBody"]))]);
    let home = Home::new(&server);
    let empty = home.root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    let output = home.run_with(&["issue", "--create"], empty.to_str().unwrap(), &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "oh-fx issue: gh CLI not found in PATH\n");
}

#[test]
fn a_reply_that_is_not_a_draft_is_never_published() {
    for (command, reply, expected) in [
        (
            "issue",
            "Title only",
            "oh-fx issue: failed to parse drafted issue title/body\n",
        ),
        (
            "issue",
            "",
            "Done.oh-fx issue: failed to parse drafted issue title/body\n",
        ),
        (
            "pr",
            "Title only",
            "oh-fx pr: failed to parse drafted PR title/body\n",
        ),
    ] {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&[reply]))]);
        let home = Home::new(&server);
        git(&home.workspace, &["init", "-q", "-b", "feature"]);
        let path = home.fake_gh();
        let output = home.run_with(&[command, "--create"], &path, &[]);
        assert_eq!(output.status.code(), Some(1), "{command} {reply:?}");
        assert_eq!(stdout(&output), "", "{command} {reply:?}");
        assert_eq!(stderr(&output), expected, "{command} {reply:?}");
        assert_eq!(home.gh_args(), None, "{command} {reply:?}");
    }
}

#[test]
fn only_the_final_reply_is_published_with_its_markdown_intact() {
    let server = FakeServer::start([
        Reply::sse(&text_then_tool_call(
            "Let me look at the branch first.",
            "call_1",
            "read_file",
            r#"{"path":"notes.txt"}"#,
        )),
        Reply::sse(&chat_text_events(&[
            "## **Add notes**\n\n## Summary\nUses `code` and **bold**.",
        ])),
    ]);
    let home = Home::new(&server);
    git(&home.workspace, &["init", "-q", "-b", "feature"]);
    fs::write(home.workspace.join("notes.txt"), "one\n").unwrap();
    let path = home.fake_gh();
    let output = home.run_with(&["pr", "--create"], &path, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "created successfully\n");
    assert_eq!(
        home.gh_args().expect("gh ran"),
        [
            "pr",
            "create",
            "--title",
            "Add notes",
            "--body",
            "## Summary\nUses `code` and **bold**."
        ]
    );
}

#[test]
fn a_failed_draft_exits_without_publishing() {
    let server = FakeServer::start([Reply::status(400, "bad request")]);
    let home = Home::new(&server);
    let path = home.fake_gh();
    let output = home.run_with(&["issue", "--create"], &path, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(home.gh_args(), None);
}

fn exit_within(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("poll oh-fx") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_signal_while_gh_publishes_ends_oh_fx_by_that_signal() {
    for (name, signal) in [("TERM", 15), ("INT", 2)] {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["Title\n\nBody"]))]);
        let home = Home::new(&server);
        let path = home.fake_gh();
        let release = home.root.join("release");
        let mut child = home
            .command(
                &["issue", "--create"],
                &path,
                &[
                    ("GH_WAIT_FILE", release.to_str().unwrap()),
                    ("GH_STDOUT", "https://github.com/o/r/issues/3"),
                ],
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn oh-fx");
        let deadline = Instant::now() + Duration::from_secs(15);
        while home.gh_args().is_none() {
            assert!(Instant::now() < deadline, "gh never started");
            thread::sleep(Duration::from_millis(20));
        }
        let killed = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(child.id().to_string())
            .status()
            .expect("run kill");
        assert!(killed.success());
        let ended = exit_within(&mut child, Duration::from_secs(5));
        fs::write(&release, "").expect("release gh");
        let status = ended.unwrap_or_else(|| child.wait().expect("wait for oh-fx"));
        let mut printed = String::new();
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut printed)
            .expect("read stdout");
        assert_eq!(status.signal(), Some(signal), "{name}: {status:?}");
        assert_eq!(printed, "", "{name}");
    }
}

#[test]
fn a_captured_draft_that_fails_exits_silently_as_upstream_does() {
    for reason in ["length", "content_filter"] {
        let events: Vec<String> = chat_text_events(&["Title\n\nBody"])
            .into_iter()
            .map(|event| {
                event.replace(
                    r#""finish_reason":"stop""#,
                    &format!(r#""finish_reason":"{reason}""#),
                )
            })
            .collect();
        let server = FakeServer::start([Reply::sse(&events)]);
        let home = Home::new(&server);
        let path = home.fake_gh();
        let output = home.run_with(&["issue", "--create"], &path, &[]);
        assert_eq!(output.status.code(), Some(1), "{reason}");
        assert_eq!(stdout(&output), "", "{reason}");
        assert_eq!(stderr(&output), "", "{reason}");
        assert_eq!(home.gh_args(), None, "{reason}");
    }
}
