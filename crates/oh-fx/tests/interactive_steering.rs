use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use ofx_contract::{INTERRUPTED_BEFORE_COMPLETION, INTERRUPTED_TURN_CONTEXT};
use ofx_testkit::{
    FakeServer, PtySession, RecordedRequest, Reply, chat_text_events, chat_tool_call_events,
};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);
const CANCELLATION: &str = "■ Cancelled";
const UP: &[u8] = b"\x1b[A";
const STEERING_OPEN: &str = "<user_steering>\nApply this live user update to the current task. Continue working unless the user asks you to stop, the task is complete, or a genuine blocker prevents progress.\n\n";
const STEERING_CLOSE: &str = "\n</user_steering>";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Home {
    fn new(base_url: &str, permission_mode: &str) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory
            .path()
            .canonicalize()
            .expect("canonicalize the temporary home");
        let workspace = root.join("workspace");
        fs::create_dir_all(root.join("config/oh-fx")).expect("create the config directory");
        fs::create_dir_all(&workspace).expect("create the workspace");
        let settings = json!({
            "provider": "local",
            "permission_mode": permission_mode,
            "yolo_acknowledged": true,
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": base_url,
                    "auth": {"type": "none"},
                    "models": ["model-a"]
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

    fn shell(&self, hint: &str) -> PtySession {
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
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .process_group(0);
        let session = PtySession::spawn(command, 30, 100).expect("spawn oh-fx in a pty");
        wait(&session, "Run /help for commands");
        wait(&session, hint);
        session
    }

    fn saved_frames(&self) -> Vec<Value> {
        let sessions = self.root.join("data/oh-fx/sessions");
        let entries: Vec<_> = fs::read_dir(&sessions)
            .expect("list the saved sessions")
            .collect();
        assert_eq!(entries.len(), 1);
        let events = entries[0]
            .as_ref()
            .expect("a session entry")
            .path()
            .join("events.jsonl");
        fs::read_to_string(events)
            .expect("read the conversation log")
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON frame"))
            .collect()
    }
}

fn wait(session: &PtySession, needle: &str) -> String {
    session
        .wait_for(WAIT, |screen| screen.contains(needle))
        .unwrap_or_else(|screen| panic!("expected {needle:?} on screen:\n{screen}"))
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn steered(text: &str) -> String {
    format!("{STEERING_OPEN}{text}{STEERING_CLOSE}")
}

fn users(request: &RecordedRequest) -> Vec<String> {
    request.json()["messages"]
        .as_array()
        .expect("chat messages")
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn in_order(screen: &str, parts: &[&str]) -> bool {
    let mut from = 0;
    parts.iter().all(|part| match screen[from..].find(part) {
        Some(found) => {
            from += found + part.len();
            true
        }
        None => false,
    })
}

#[test]
fn text_typed_while_a_reply_streams_steers_that_turn_without_cancelling_it() {
    let held = Reply::held_sse(
        &chat_text_events(&["Looking at the parser.\nStill reading.\n", "never"])[..2],
    );
    let server = FakeServer::start([
        held,
        Reply::sse(&chat_text_events(&["Checked the tests too."])),
    ]);
    let home = Home::new(&server.base_url(), "auto");
    let mut session = home.shell("auto · model-a");
    session.send(b"fix the parser\r");
    wait(&session, "Looking at the parser.");
    session.send(b"check the tests too\r");
    let screen = wait(&session, "Checked the tests too.");
    assert!(!screen.contains(CANCELLATION), "{screen}");
    assert!(!screen.contains("never"), "{screen}");
    assert!(
        in_order(
            &screen,
            &[
                "┃ fix the parser",
                "Looking at the parser.",
                "Still reading.",
                "┃ check the tests too",
                "Checked the tests too."
            ]
        ),
        "{screen}"
    );
    assert_eq!(screen.matches("check the tests too").count(), 1, "{screen}");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        users(&requests[1]),
        ["fix the parser".to_owned(), steered("check the tests too")]
    );
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    let events: Vec<String> = home
        .saved_frames()
        .iter()
        .map(|frame| {
            frame["event"]
                .as_object()
                .expect("an event object")
                .keys()
                .next()
                .expect("an event kind")
                .clone()
        })
        .collect();
    assert_eq!(
        events,
        [
            "user",
            "assistant",
            "steering",
            "assistant",
            "turn_completed"
        ]
    );
    assert_eq!(
        home.saved_frames()[2]["event"],
        json!({"steering": {"text": "check the tests too"}})
    );
}

#[test]
fn text_typed_while_a_shell_command_runs_waits_for_its_result_and_up_pulls_it_back() {
    let run = json!({"request": {"action": "run", "command": "sh check.sh"}}).to_string();
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events("call-1", "shell", &run)),
        Reply::sse(&chat_text_events(&["STEERED_DONE"])),
    ]);
    let home = Home::new(&server.base_url(), "yolo");
    fs::write(
        home.workspace.join("check.sh"),
        "printf started > started\nwhile [ ! -f release ]; do sleep 0.05; done\nprintf done > finished\n",
    )
    .expect("write the check script");
    let mut session = home.shell("full access · model-a");
    session.send(b"run the check\r");
    wait_for_file(&home.workspace.join("started"));
    session.send(b"change the header tone\r");
    wait(&session, "┋ change the header tone");
    session.send(UP);
    session
        .wait_for(WAIT, |screen| {
            screen.contains("┃ change the header tone") && !screen.contains("┋ change")
        })
        .unwrap_or_else(|screen| panic!("the steer returns to the composer:\n{screen}"));
    session.send(b" and keep it short\r");
    wait(&session, "┋ change the header tone and keep it short");
    assert_eq!(server.requests().len(), 1);
    fs::write(home.workspace.join("release"), "go").expect("release the command");
    let screen = wait(&session, "STEERED_DONE");
    wait_for_file(&home.workspace.join("finished"));
    assert!(!screen.contains(CANCELLATION), "{screen}");
    assert!(
        in_order(
            &screen,
            &[
                "┃ run the check",
                "┃ change the header tone and keep it short",
                "STEERED_DONE"
            ]
        ),
        "{screen}"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = requests[1].json();
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .expect("chat messages")
        .iter()
        .filter_map(|message| message["role"].as_str())
        .collect();
    assert_eq!(roles[roles.len() - 2..], ["tool", "user"]);
    assert_eq!(
        users(&requests[1]).last(),
        Some(&steered("change the header tone and keep it short"))
    );
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn a_cancelled_turn_saves_the_steering_it_took_after_a_tool_result_once() {
    let hold = |step: &str| {
        json!({"request": {"action": "run", "command": format!("sh hold.sh {step}")}}).to_string()
    };
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events("call-1", "shell", &hold("one"))),
        Reply::sse(&chat_tool_call_events("call-2", "shell", &hold("two"))),
    ]);
    let home = Home::new(&server.base_url(), "yolo");
    fs::write(
        home.workspace.join("hold.sh"),
        "printf started > \"started-$1\"\nwhile [ ! -f \"release-$1\" ]; do sleep 0.05; done\n",
    )
    .expect("write the hold script");
    let mut session = home.shell("full access · model-a");
    session.send(b"run both steps\r");
    wait_for_file(&home.workspace.join("started-one"));
    session.send(b"STEERING_FIRST\r");
    wait(&session, "┋ STEERING_FIRST");
    fs::write(home.workspace.join("release-one"), "go").expect("release the first step");
    wait_for_file(&home.workspace.join("started-two"));
    session.send(b"\x03");
    wait(&session, CANCELLATION);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(users(&requests[1]).last(), Some(&steered("STEERING_FIRST")));
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).is_some());
    let frames = home.saved_frames();
    let saved = serde_json::to_string(&frames).expect("serialize the saved frames");
    assert_eq!(saved.matches("STEERING_FIRST").count(), 1, "{saved}");
    let kinds: Vec<String> = frames
        .iter()
        .map(|frame| {
            frame["event"]
                .as_object()
                .expect("an event object")
                .keys()
                .next()
                .expect("an event kind")
                .clone()
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "user",
            "tool_call",
            "tool_result",
            "steering",
            "tool_call",
            "tool_result",
            "interrupted"
        ]
    );
    fs::write(home.workspace.join("release-two"), "go").expect("release the second step");
}

#[test]
fn a_live_cancelled_reply_closes_before_the_next_plain_prompt() {
    let held =
        Reply::held_sse(&chat_text_events(&["Half of the reply.\nStill reading.\n", "never"])[..2]);
    let server = FakeServer::start([held, Reply::sse(&chat_text_events(&["The next answer."]))]);
    let home = Home::new(&server.base_url(), "auto");
    let mut session = home.shell("model-a");
    session.send(b"first question\r");
    wait(&session, "Half of the reply.");
    session.send(b"\x03");
    wait(&session, CANCELLATION);
    session.send(b"what happened?\r");
    wait(&session, "The next answer.");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let messages = requests[1].json()["messages"].as_array().unwrap().clone();
    let first = messages
        .iter()
        .position(|message| message["content"] == "first question")
        .unwrap();
    assert_eq!(messages[first + 1]["role"], "assistant");
    assert_eq!(
        messages[first + 1]["content"],
        format!("Half of the reply.\nStill reading.\n\n\n{INTERRUPTED_BEFORE_COMPLETION}")
    );
    assert_eq!(messages[first + 2]["role"], "user");
    assert_eq!(messages[first + 2]["content"], INTERRUPTED_TURN_CONTEXT);
    assert_eq!(messages[first + 3]["content"], "what happened?");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).is_some());
}
