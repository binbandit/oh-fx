use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use ofx_testkit::{
    FakeServer, Gate, PtySession, RecordedRequest, Reply, chat_text_events, chat_tool_call_events,
};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);
const APPROVAL_ARMING: Duration = Duration::from_millis(700);
const PERMISSION_NEEDED: &str = "Permission needed · Choose one";
const CANCELLATION: &str = "■ Cancelled · What can oh-fx do differently?";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Home {
    fn new(base_url: &str) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        let config = root.join("config/oh-fx");
        let workspace = root.join("workspace");
        fs::create_dir_all(&config).expect("create the config directory");
        fs::create_dir_all(&workspace).expect("create the workspace");
        let settings = json!({
            "provider": "local",
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": base_url,
                    "auth": {"type": "none"},
                    "models": ["model-a"]
                }
            }
        });
        fs::write(config.join("settings.json"), settings.to_string()).expect("write settings.json");
        Self {
            _directory: directory,
            root,
            workspace,
        }
    }

    fn shell(&self) -> PtySession {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .process_group(0);
        let session = PtySession::spawn(command, 30, 100).expect("spawn oh-fx in a pty");
        wait(&session, "auto · model-a");
        session
    }

    fn marker(&self) -> bool {
        self.workspace.join("marker").exists()
    }
}

fn wait(session: &PtySession, needle: &str) -> String {
    session
        .wait_for(WAIT, |screen| screen.contains(needle))
        .unwrap_or_else(|screen| panic!("expected {needle:?} on screen:\n{screen}"))
}

fn exit(mut session: PtySession) {
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

fn run_marker() -> Reply {
    Reply::sse(&chat_tool_call_events(
        "call-1",
        "shell",
        r#"{"request":{"action":"run","command":"touch marker"}}"#,
    ))
}

fn decision(arguments: &str) -> Reply {
    Reply::sse(&chat_tool_call_events(
        "review-1",
        "permission_decision",
        arguments,
    ))
}

fn last_tool_result(request: &RecordedRequest) -> String {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("a tool result")
        .to_owned()
}

fn is_review(request: &RecordedRequest) -> bool {
    request.json()["tools"][0]["function"]["name"] == "permission_decision"
}

#[test]
fn a_cleared_review_runs_the_command_without_asking() {
    let server = FakeServer::start([
        run_marker(),
        decision(r#"{"decision":"clear"}"#),
        Reply::sse(&chat_text_events(&["Made the marker."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell();
    session.send(b"create the marker\r");
    let screen = wait(&session, "Made the marker.");
    assert!(!screen.contains("Permission needed"), "{screen}");
    assert!(home.marker());
    let requests = server.requests();
    assert!(is_review(&requests[1]));
    assert_eq!(
        requests[1].json()["messages"][1]["content"],
        "review_context_kind: contextual\ntrusted_root_context:\ncurrent_request: create the marker\n"
    );
    exit(session);
}

#[test]
fn a_command_is_shown_running_while_its_review_is_pending() {
    let gate = Gate::default();
    let server = FakeServer::start([
        run_marker(),
        decision(r#"{"decision":"clear"}"#).after(&gate),
        Reply::sse(&chat_text_events(&["Made the marker."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell();
    session.send(b"create the marker\r");
    let started = Instant::now();
    while server.requests().len() < 2 {
        assert!(started.elapsed() < WAIT, "the review never started");
        thread::sleep(Duration::from_millis(20));
    }
    let screen = wait(&session, "• Running (");
    assert!(!home.marker(), "{screen}");
    gate.open();
    wait(&session, "Made the marker.");
    assert!(home.marker());
    exit(session);
}

#[test]
fn a_caution_holds_the_command_without_asking() {
    let server = FakeServer::start([
        run_marker(),
        decision(r#"{"decision":"caution","rationale":"Nothing asked for a marker."}"#),
        Reply::sse(&chat_text_events(&["It was held."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell();
    session.send(b"create the marker\r");
    let screen = wait(&session, "It was held.");
    assert!(!screen.contains("Permission needed"), "{screen}");
    assert!(!home.marker());
    let held: Value =
        serde_json::from_str(&last_tool_result(&server.requests()[2])).expect("the hold is JSON");
    assert_eq!(held["error"]["reason"], "review_caution");
    assert_eq!(held["error"]["advice"], "Nothing asked for a marker.");
    exit(session);
}

#[test]
fn a_malformed_review_asks_the_user_and_runs_once_approved() {
    let server = FakeServer::start([
        run_marker(),
        Reply::sse(&chat_text_events(&[r#"{"decision":"clear"}"#])),
        Reply::sse(&chat_text_events(&["Looks fine."])),
        Reply::sse(&chat_text_events(&["Made the marker."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell();
    session.send(b"create the marker\r");
    let screen = wait(&session, PERMISSION_NEEDED);
    assert!(screen.contains("$ touch marker"), "{screen}");
    assert!(!home.marker());
    thread::sleep(APPROVAL_ARMING);
    session.send(b"1");
    wait(&session, "Made the marker.");
    assert!(home.marker());
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert!(is_review(&requests[1]) && is_review(&requests[2]));
    assert!(last_tool_result(&requests[3]).contains("exit_code"));
    exit(session);
}

#[test]
fn a_failing_reviewer_asks_the_user_and_a_denial_reaches_the_model() {
    let server = FakeServer::start([
        run_marker(),
        Reply::status(503, r#"{"error":{"message":"reviewer unavailable"}}"#),
        Reply::sse(&chat_text_events(&["Left it alone."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell();
    session.send(b"create the marker\r");
    wait(&session, PERMISSION_NEEDED);
    session.send(b"3");
    wait(&session, "Left it alone.");
    assert!(!home.marker());
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(last_tool_result(&requests[2]).contains("tool_permission_denied"));
    exit(session);
}

#[test]
fn an_approval_never_runs_the_command_in_a_directory_replaced_while_the_prompt_was_open() {
    let replacements: [fn(&Path, &Path); 2] = [
        |build, outside| {
            fs::rename(build, build.with_file_name("reviewed"))
                .expect("move the reviewed directory");
            symlink(outside, build).expect("link the outside directory");
        },
        |build, _| {
            let inode = |directory: &Path| fs::metadata(directory).expect("read").ino();
            let reviewed = inode(build);
            for _ in 0..100 {
                fs::remove_dir_all(build).expect("remove the reviewed directory");
                fs::create_dir(build).expect("create a replacement directory");
                if inode(build) == reviewed {
                    break;
                }
            }
            fs::write(build.join("marker"), "unreviewed").expect("write the replacement marker");
        },
    ];
    for replace in replacements {
        let server = FakeServer::start([
            Reply::sse(&chat_tool_call_events(
                "call-1",
                "shell",
                r#"{"request":{"action":"run","command":"rm -f marker","cwd":"build"}}"#,
            )),
            Reply::status(503, r#"{"error":{"message":"reviewer unavailable"}}"#),
            Reply::sse(&chat_text_events(&["Stopped."])),
        ]);
        let home = Home::new(&server.base_url());
        let build = home.workspace.join("build");
        let outside = home.root.join("outside");
        for directory in [&build, &outside] {
            fs::create_dir(directory).expect("create a directory");
        }
        fs::write(build.join("marker"), "reviewed").expect("write the reviewed marker");
        fs::write(outside.join("marker"), "unreviewed").expect("write the outside marker");
        let session = home.shell();
        session.send(b"remove the build marker\r");
        wait(&session, PERMISSION_NEEDED);
        replace(&build, &outside);
        thread::sleep(APPROVAL_ARMING);
        session.send(b"1");
        wait(&session, "Stopped.");
        for unreviewed in [outside.join("marker"), build.join("marker")] {
            assert_eq!(
                fs::read_to_string(&unreviewed).expect("the unreviewed marker is kept"),
                "unreviewed"
            );
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            last_tool_result(&requests[2]),
            r#"{"error":{"tool":"shell","code":"CommandAuthorityContextMismatch","retryable":false}}"#
        );
        exit(session);
    }
}

#[test]
fn ctrl_c_during_a_review_cancels_the_turn_without_running_the_command() {
    let stalled = Reply::held_sse(&[
        r#"{"id":"x","object":"chat.completion.chunk","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
    ]);
    let server = FakeServer::start([
        run_marker(),
        stalled,
        Reply::sse(&chat_text_events(&["Fresh."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell();
    session.send(b"create the marker\r");
    let started = Instant::now();
    while server.requests().len() < 2 {
        assert!(started.elapsed() < WAIT, "the review never started");
        thread::sleep(Duration::from_millis(20));
    }
    session.send(b"\x03");
    wait(&session, CANCELLATION);
    assert!(!home.marker());
    session.send(b"\x1b");
    wait(&session, "auto · model-a");
    session.send(b"again\r");
    wait(&session, "Fresh.");
    assert!(!home.marker());
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(is_review(&requests[1]));
    assert!(!is_review(&requests[2]));
    exit(session);
}
