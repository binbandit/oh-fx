use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ofx_testkit::{
    FakeServer, PtySession, RecordedRequest, Reply, chat_text_events, chat_tool_call_events,
};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);
const WELCOME: &str = "Run /help for commands";
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
        let workspace = root.join("workspace");
        fs::create_dir_all(root.join("config/oh-fx")).expect("create the config directory");
        fs::create_dir_all(&workspace).expect("create the workspace");
        fs::write(
            root.join("config/oh-fx/settings.json"),
            settings(base_url).to_string(),
        )
        .expect("write settings.json");
        Self {
            _directory: directory,
            root,
            workspace,
        }
    }

    fn command_in(&self, workspace: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
            .current_dir(workspace)
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
        command
    }

    fn spawn(&self, args: &[&str]) -> PtySession {
        self.spawn_in(&self.workspace, args)
    }

    fn spawn_in(&self, workspace: &Path, args: &[&str]) -> PtySession {
        PtySession::spawn(self.command_in(workspace, args), 30, 100).expect("spawn oh-fx in a pty")
    }

    fn shell(&self, args: &[&str], ready: &str) -> PtySession {
        let session = self.spawn(args);
        wait(&session, ready);
        wait(&session, "auto · model-a");
        session
    }

    fn sessions(&self) -> PathBuf {
        self.root.join("data/oh-fx/sessions")
    }

    fn session_ids(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.sessions()) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .map(|entry| {
                entry
                    .expect("a session entry")
                    .file_name()
                    .into_string()
                    .expect("a UTF-8 session id")
            })
            .collect();
        ids.sort();
        ids
    }

    fn only_session(&self) -> String {
        let ids = self.session_ids();
        assert_eq!(ids.len(), 1, "{ids:?}");
        ids[0].clone()
    }

    fn events(&self, id: &str) -> PathBuf {
        self.sessions().join(id).join("events.jsonl")
    }

    fn frames(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(self.events(id))
            .expect("read the conversation log")
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON frame"))
            .collect()
    }

    fn metadata(&self, id: &str) -> Value {
        serde_json::from_slice(
            &fs::read(self.sessions().join(id).join("session.json")).expect("read session.json"),
        )
        .expect("session metadata")
    }

    fn remembered(&self) -> Option<String> {
        let directory = fs::read_dir(self.root.join("data/oh-fx/continue")).ok()?;
        let entries: Vec<_> = directory.collect();
        assert_eq!(entries.len(), 1);
        let path = entries[0].as_ref().expect("a continue entry").path();
        Some(
            fs::read_to_string(path)
                .expect("read the remembered session")
                .trim_end()
                .to_owned(),
        )
    }

    fn append(&self, id: &str, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(self.events(id))
            .expect("open the conversation log")
            .write_all(bytes)
            .expect("append to the conversation log");
    }
}

fn settings(base_url: &str) -> Value {
    json!({
        "provider": "local",
        "providers": {
            "local": {
                "protocol": "openai-chat-completions",
                "base_url": base_url,
                "auth": {"type": "none"},
                "models": ["model-a", "vendor/model-b"]
            }
        }
    })
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

fn fails_with(mut session: PtySession, line: &str) {
    let status = session.wait_exit(WAIT).expect("oh-fx exits");
    assert_eq!(status.code(), Some(1), "{status:?}");
    assert!(session.drain_output(WAIT), "the terminal never closed");
    let output = String::from_utf8_lossy(&session.output()).into_owned();
    assert!(output.contains(&format!("{line}\r\n")), "{output:?}");
    assert!(!output.contains(WELCOME), "{output:?}");
}

fn chat(request: &RecordedRequest) -> Vec<(String, String)> {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] != "system")
        .map(|message| {
            (
                message["role"].as_str().expect("a role").to_owned(),
                message["content"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn frame(seq: u64, event: &Value) -> Vec<u8> {
    let mut line =
        json!({"schema_version": 3, "seq": seq, "timestamp_ms": 1, "event": event}).to_string();
    line.push('\n');
    line.into_bytes()
}

fn kinds(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| {
            frame["event"]
                .as_object()
                .and_then(|event| event.keys().next())
                .expect("an event kind")
                .clone()
        })
        .collect()
}

fn turn(prompt: &str, reply: &str) -> Vec<(String, String)> {
    vec![
        ("user".to_owned(), prompt.to_owned()),
        ("assistant".to_owned(), reply.to_owned()),
    ]
}

#[test]
fn a_shell_saves_its_turns_and_continue_reopens_them_in_the_scrollback() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["First **answer**."])),
        Reply::sse(&chat_text_events(&["Second answer."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first question\r");
    wait(&session, "First answer.");
    exit(session);
    let id = home.only_session();
    assert_eq!(
        kinds(&home.frames(&id)),
        ["user", "assistant", "turn_completed"]
    );
    assert_eq!(home.remembered(), Some(id.clone()));
    assert_eq!(home.metadata(&id)["model"], "model-a");

    let session = home.shell(&["-c"], "session resumed: first question");
    let screen = wait(&session, "First answer.");
    assert!(!screen.contains(WELCOME), "{screen}");
    assert!(
        screen.contains("* session resumed: first question\n\n┃ first question\n\n  First answer."),
        "{screen}"
    );
    session.send(b"second question\r");
    wait(&session, "Second answer.");
    exit(session);
    assert_eq!(home.session_ids(), std::slice::from_ref(&id));
    assert_eq!(
        chat(&server.requests()[1]),
        [
            turn("first question", "First **answer**."),
            vec![("user".to_owned(), "second question".to_owned())]
        ]
        .concat()
    );
    assert_eq!(
        kinds(&home.frames(&id)),
        [
            "user",
            "assistant",
            "turn_completed",
            "user",
            "assistant",
            "turn_completed"
        ]
    );
}

#[test]
fn a_shell_left_without_a_prompt_saves_nothing_and_continue_explains_why() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"/fast\r");
    wait(
        &session,
        "* fast: This model does not come with a fast mode.",
    );
    exit(session);
    assert!(home.session_ids().is_empty());
    assert_eq!(home.remembered(), None);
    fails_with(
        home.spawn(&["-c"]),
        "oh-fx: no remembered session for this workspace; choose one with oh-fx -r or oh-fx --resume <id>",
    );
    fails_with(
        home.spawn(&["resume"]),
        "oh-fx: no saved sessions for this workspace.",
    );
    fails_with(
        home.spawn(&["--resume", "missing-id"]),
        "oh-fx: saved session not found.",
    );
}

#[test]
fn a_requested_resume_needs_the_session_store_but_a_plain_launch_runs_without_it() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Unsaved."]))]);
    let home = Home::new(&server.base_url());
    fs::create_dir_all(home.root.join("data")).expect("create the data home");
    fs::write(home.root.join("data/oh-fx"), "not a directory").expect("block the store");
    for args in [&["-r"][..], &["-c"], &["resume"], &["--resume", "some-id"]] {
        fails_with(home.spawn(args), "oh-fx: SessionPathUnsafe");
    }
    let session = home.shell(&[], WELCOME);
    session.send(b"still answered\r");
    wait(&session, "Unsaved.");
    exit(session);
}

#[test]
fn resume_targets_reopen_a_session_by_id_or_the_latest_and_remember_it() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Answer one."])),
        Reply::sse(&chat_text_events(&["Answer two."])),
        Reply::sse(&chat_text_events(&["Again one."])),
        Reply::sse(&chat_text_events(&["Again two."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"topic one\r");
    wait(&session, "Answer one.");
    session.send(b"/new\r");
    wait(&session, WELCOME);
    session.send(b"topic two\r");
    wait(&session, "Answer two.");
    exit(session);
    let ids = home.session_ids();
    assert_eq!(ids.len(), 2, "{ids:?}");
    let first = ids
        .iter()
        .find(|id| home.frames(id)[0]["event"]["user"]["text"] == "topic one")
        .expect("the first session")
        .clone();
    let second = ids
        .iter()
        .find(|id| **id != first)
        .expect("the second session")
        .clone();
    assert_eq!(home.remembered(), Some(second.clone()));

    let session = home.shell(&["resume", &first], "session resumed: topic one");
    let screen = wait(&session, "Answer one.");
    assert!(!screen.contains("topic two"), "{screen}");
    session.send(b"more one\r");
    wait(&session, "Again one.");
    exit(session);
    assert_eq!(home.remembered(), Some(first.clone()));

    let session = home.shell(&["resume"], "session resumed: topic one");
    session.send(b"more again\r");
    wait(&session, "Again two.");
    exit(session);
    assert_eq!(home.frames(&first).len(), 9);
    assert_eq!(home.frames(&second).len(), 3);
    assert_eq!(
        chat(&server.requests()[3]),
        [
            turn("topic one", "Answer one."),
            turn("more one", "Again one."),
            vec![("user".to_owned(), "more again".to_owned())]
        ]
        .concat()
    );
}

#[test]
fn clear_and_new_start_fresh_sessions_and_keep_the_finished_ones() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Kept."])),
        Reply::sse(&chat_text_events(&["Fresh."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"keep this\r");
    wait(&session, "Kept.");
    session.send(b"/clear\r");
    session.send(b"/new\r");
    session.send(b"start over\r");
    wait(&session, "Fresh.");
    exit(session);
    assert_eq!(home.session_ids().len(), 2);
    assert_eq!(
        chat(&server.requests()[1]),
        [("user".to_owned(), "start over".to_owned())]
    );
}

#[test]
fn a_session_resumed_from_another_workspace_moves_here() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Elsewhere."])),
        Reply::sse(&chat_text_events(&["Here."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"from the first workspace\r");
    wait(&session, "Elsewhere.");
    exit(session);
    let id = home.only_session();
    let other = home.root.join("other");
    fs::create_dir_all(&other).expect("create another workspace");
    let session = home.spawn_in(&other, &["resume", &id]);
    wait(&session, "Elsewhere.");
    session.send(b"continue here\r");
    wait(&session, "Here.");
    exit(session);
    let metadata = home.metadata(&id);
    let other = fs::canonicalize(&other).expect("canonicalize the other workspace");
    assert_eq!(
        metadata["workspace_root"],
        other.to_str().expect("a UTF-8 path")
    );
    assert_eq!(
        metadata["origin_workspace_root"],
        fs::canonicalize(&home.workspace)
            .expect("canonicalize the workspace")
            .to_str()
            .expect("a UTF-8 path")
    );
}

#[test]
fn a_session_open_in_another_shell_is_reported_busy() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Held."]))]);
    let home = Home::new(&server.base_url());
    let holder = home.shell(&[], WELCOME);
    holder.send(b"hold it\r");
    wait(&holder, "Held.");
    let id = home.only_session();
    fails_with(
        home.spawn(&["resume", &id]),
        "oh-fx: another oh-fx process may be using this session (running or suspended); check other terminals or run jobs, then use fg or quit that process",
    );
    exit(holder);
}

#[test]
fn torn_and_unfinished_turns_left_by_a_crash_reopen_cleanly() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Saved."])),
        Reply::sse(&chat_text_events(&["Recovered."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"saved turn\r");
    wait(&session, "Saved.");
    exit(session);
    let id = home.only_session();
    let torn = b"{\"schema_version\":3,\"seq\":6,";
    home.append(
        &id,
        &[
            frame(4, &json!({"user": {"text": "torn turn"}})),
            torn.to_vec(),
        ]
        .concat(),
    );
    let session = home.shell(&["resume", &id], "session resumed: saved turn");
    let screen = wait(&session, "Saved.");
    assert!(!screen.contains("torn turn"), "{screen}");
    exit(session);
    assert_eq!(
        kinds(&home.frames(&id)),
        ["user", "assistant", "turn_completed"]
    );
    let checkpoint =
        json!({"context_checkpoint": {"covers_through_seq": 4, "summary": "earlier work"}});
    home.append(
        &id,
        &[
            frame(4, &json!({"user": {"text": "unfinished turn"}})),
            frame(5, &checkpoint),
            torn.to_vec(),
        ]
        .concat(),
    );
    let session = home.shell(&["resume", &id], "session resumed: unfinished turn");
    let screen = wait(&session, "system: failed");
    assert!(
        screen.contains("  Saved.\n\n┃ unfinished turn\n\n"),
        "{screen}"
    );
    session.send(b"after the crash\r");
    wait(&session, "Recovered.");
    exit(session);
    assert_eq!(
        kinds(&home.frames(&id))[3..],
        [
            "user",
            "context_checkpoint",
            "interrupted",
            "user",
            "assistant",
            "turn_completed"
        ]
    );
}

#[test]
fn saved_text_cannot_drive_the_terminal_when_it_is_replayed() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&[
        "reply \u{1b}]2;owned\u{7} \u{1b}[2J done",
    ]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"title \x1b]0;pwned\x07 prompt\r");
    wait(&session, "done");
    exit(session);
    let id = home.only_session();
    let session = home.shell(&["-c"], "session resumed");
    wait(&session, "done");
    let output = session.output();
    for injected in [&b"\x1b]2;owned"[..], b"\x1b]0;pwned", b"\x1b[2J done"] {
        assert!(
            !output
                .windows(injected.len())
                .any(|window| window == injected),
            "{:?}",
            String::from_utf8_lossy(&output)
        );
    }
    exit(session);
    assert_eq!(home.session_ids(), [id]);
}

#[test]
fn a_cancelled_turn_is_replayed_with_its_cancellation() {
    let server = FakeServer::start([Reply::held_sse(
        &chat_text_events(&["Partial reply.\nSecond line.\n", "never shown"])[..2],
    )]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"stop me\r");
    wait(&session, "Partial reply.");
    session.send(b"\x03");
    wait(&session, CANCELLATION);
    exit(session);
    let session = home.shell(&["-c"], "session resumed: stop me");
    let screen = wait(&session, CANCELLATION);
    assert!(
        screen.contains(&format!(
            "┃ stop me\n\n  Partial reply.\n  Second line.\n\n{CANCELLATION}"
        )),
        "{screen}"
    );
    exit(session);
}

#[test]
fn a_chosen_model_is_saved_and_resumed_while_launch_flags_are_not_saved() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["On b."])),
        Reply::sse(&chat_text_events(&["Still b."])),
        Reply::sse(&chat_text_events(&["Flagged a."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"/model vendor/model-b\r");
    wait(&session, "auto · model-b");
    session.send(b"use b\r");
    wait(&session, "On b.");
    exit(session);
    let id = home.only_session();
    assert_eq!(home.metadata(&id)["model"], "vendor/model-b");
    let session = home.spawn(&["-c"]);
    wait(&session, "auto · model-b");
    session.send(b"again\r");
    wait(&session, "Still b.");
    exit(session);
    let session = home.spawn(&["--model", "model-a", "-c"]);
    wait(&session, "auto · model-a");
    session.send(b"flagged\r");
    wait(&session, "Flagged a.");
    exit(session);
    let models: Vec<Value> = server
        .requests()
        .iter()
        .map(|request| request.json()["model"].clone())
        .collect();
    assert_eq!(models, ["vendor/model-b", "vendor/model-b", "model-a"]);
    assert_eq!(home.metadata(&id)["model"], "vendor/model-b");
}

#[test]
fn a_session_whose_provider_changed_is_refused_before_the_shell_opens() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Saved."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"keep this\r");
    wait(&session, "Saved.");
    exit(session);
    let id = home.only_session();
    fs::write(
        home.root.join("config/oh-fx/settings.json"),
        settings("http://127.0.0.1:9/v1").to_string(),
    )
    .expect("rewrite settings.json");
    fails_with(
        home.spawn(&["resume", &id]),
        "oh-fx: ConfiguredProviderChanged",
    );
    fails_with(home.spawn(&["-c"]), "oh-fx: ConfiguredProviderChanged");
    assert_eq!(server.requests().len(), 1);
    assert_eq!(kinds(&home.frames(&id)).len(), 3);
}

#[test]
fn a_long_session_reopens_with_its_latest_turns_on_screen() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Seed reply."])),
        Reply::sse(&chat_text_events(&["Still here."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"seed prompt\r");
    wait(&session, "Seed reply.");
    exit(session);
    let id = home.only_session();
    let template = home.frames(&id);
    let filler = "lorem ipsum ".repeat(20);
    let mut appended = Vec::new();
    for turn in 1..=1500_u64 {
        for (offset, frame) in (1..).zip(&template) {
            let mut frame = frame.clone();
            frame["seq"] = json!(turn * 3 + offset);
            if let Some(user) = frame["event"].get_mut("user") {
                user["text"] = json!(format!("prompt {turn}"));
            }
            if let Some(assistant) = frame["event"].get_mut("assistant") {
                assistant["text"] = json!(format!("{filler}reply {turn}"));
            }
            appended.extend_from_slice(frame.to_string().as_bytes());
            appended.push(b'\n');
        }
    }
    home.append(&id, &appended);
    let session = home.shell(&["-c"], "reply 1500");
    session.send(b"after the history\r");
    wait(&session, "Still here.");
    exit(session);
    let request = &server.requests()[1];
    let messages = chat(request);
    assert_eq!(messages.len(), 2 * 1501 + 1);
    assert_eq!(messages[3000].1, "prompt 1500");
    assert_eq!(home.frames(&id).len(), 3 * 1502);
}

#[test]
fn a_turn_whose_tool_result_cannot_be_saved_after_a_checkpoint_blocks_the_next_prompt() {
    let big = format!("{}LAST_SENTINEL", "word ".repeat(30_000));
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&[&big])),
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "read_file",
            r#"{"path":"small.txt"}"#,
        )),
        Reply::sse(&chat_text_events(&["Read it."])),
        Reply::sse(&chat_text_events(&["Fourth."])),
    ]);
    let home = Home::new(&server.base_url());
    let mut windowed = settings(&server.base_url());
    windowed["providers"]["local"]["model_metadata"] =
        json!({"model-a": {"context_window": 40000, "max_output_tokens": 1000}});
    fs::write(
        home.root.join("config/oh-fx/settings.json"),
        windowed.to_string(),
    )
    .expect("rewrite settings.json");
    fs::write(home.workspace.join("small.txt"), "alpha\n").expect("write small.txt");
    let session = home.shell(&[], WELCOME);
    session.send(b"first\r");
    wait(&session, "LAST_SENTINEL");
    session
        .wait_for(WAIT, |_| {
            home.session_ids().len() == 1 && home.frames(&home.only_session()).len() == 3
        })
        .unwrap_or_else(|screen| panic!("the first turn was not saved:\n{screen}"));
    let id = home.only_session();
    fs::write(home.sessions().join(&id).join("tool-results"), "blocked")
        .expect("block the tool results");
    session.send(b"second\r");
    wait(
        &session,
        "could not save it (SessionPathUnsafe). New messages are blocked",
    );
    let saved = kinds(&home.frames(&id));
    assert_eq!(
        saved,
        [
            "user",
            "assistant",
            "turn_completed",
            "user",
            "context_checkpoint"
        ]
    );
    fs::remove_file(home.sessions().join(&id).join("tool-results"))
        .expect("unblock the tool results");
    session.send(b"third\r");
    wait(&session, "SessionCommitFailed");
    exit(session);
    assert_eq!(server.requests().len(), 3);
    assert_eq!(kinds(&home.frames(&id)), saved);
    let session = home.shell(&["-c"], "┃ second\n\n✗ system: failed");
    session.send(b"fourth\r");
    wait(&session, "Fourth.");
    exit(session);
    assert_eq!(
        kinds(&home.frames(&id))[5..],
        ["interrupted", "user", "assistant", "turn_completed"]
    );
    let resumed = chat(&server.requests()[3]);
    assert!(
        resumed.iter().all(|(_, text)| text != "third"),
        "{resumed:?}"
    );
    assert_eq!(
        resumed.last(),
        Some(&("user".to_owned(), "fourth".to_owned()))
    );
}

#[test]
fn a_manual_compaction_is_saved_and_a_resumed_session_continues_from_its_checkpoint() {
    let server = FakeServer::start(
        (1..=7).map(|turn| Reply::sse(&chat_text_events(&[&format!("answer {turn}")]))),
    );
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    for turn in 1..=5 {
        session.send(format!("question {turn}\r").as_bytes());
        wait(&session, &format!("answer {turn}"));
    }
    session.send(b"/compact\r");
    session.send(b"question 6\r");
    wait(&session, "answer 6");
    exit(session);
    let id = home.only_session();
    let frames = home.frames(&id);
    assert_eq!(
        kinds(&frames)[15..],
        ["context_checkpoint", "user", "assistant", "turn_completed"]
    );
    assert_eq!(
        frames[15]["event"]["context_checkpoint"]["covers_through_seq"],
        3
    );
    let live = chat(&server.requests()[5]);
    assert!(
        live[0].1.starts_with("<compacted_conversation>\n"),
        "{live:?}"
    );
    assert!(
        live.iter().all(|(_, text)| text != "question 1"),
        "{live:?}"
    );

    let session = home.shell(&["-c"], "session resumed");
    session.send(b"question 7\r");
    wait(&session, "answer 7");
    exit(session);
    assert_eq!(
        chat(&server.requests()[6]),
        [
            live,
            vec![
                ("assistant".to_owned(), "answer 6".to_owned()),
                ("user".to_owned(), "question 7".to_owned())
            ]
        ]
        .concat()
    );
}

#[test]
fn a_manual_compaction_after_an_unsaved_turn_covers_only_the_saved_turns_it_summarized() {
    let replies = [
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "read_file",
            r#"{"path":"small.txt"}"#,
        )),
        Reply::sse(&chat_text_events(&["answer 1"])),
    ]
    .into_iter()
    .chain((2..=6).map(|turn| Reply::sse(&chat_text_events(&[&format!("answer {turn}")]))))
    .chain([
        Reply::sse(&chat_text_events(&[
            "Turn 1\nIn between: Read the file.\nT1: read small.txt",
        ])),
        Reply::sse(&chat_text_events(&["answer 7"])),
        Reply::sse(&chat_text_events(&["answer 8"])),
    ]);
    let server = FakeServer::start(replies);
    let home = Home::new(&server.base_url());
    fs::write(home.workspace.join("small.txt"), "alpha\n").expect("write small.txt");
    let session = home.shell(&[], WELCOME);
    session
        .wait_for(WAIT, |_| home.session_ids().len() == 1)
        .unwrap_or_else(|screen| panic!("the session was not started:\n{screen}"));
    let id = home.only_session();
    let results = home.sessions().join(&id).join("tool-results");
    fs::write(&results, "blocked").expect("block the tool results");
    session.send(b"question 1\r");
    wait(&session, "could not save it (SessionPathUnsafe)");
    fs::remove_file(&results).expect("unblock the tool results");
    for turn in 2..=6 {
        session.send(format!("question {turn}\r").as_bytes());
        wait(&session, &format!("answer {turn}"));
    }
    session.send(b"/compact\r");
    session.send(b"question 7\r");
    wait(&session, "answer 7");
    exit(session);
    let frames = home.frames(&id);
    assert_eq!(kinds(&frames)[15], "context_checkpoint");
    assert_eq!(
        frames[15]["event"]["context_checkpoint"]["covers_through_seq"],
        3
    );
    let live = chat(&server.requests()[8]);
    assert!(
        live[0].1.starts_with("<compacted_conversation>\n"),
        "{live:?}"
    );
    assert_eq!(live[1], ("user".to_owned(), "question 3".to_owned()));

    let session = home.shell(&["-c"], "session resumed");
    session.send(b"question 8\r");
    wait(&session, "answer 8");
    exit(session);
    assert_eq!(
        chat(&server.requests()[9]),
        [
            live,
            vec![
                ("assistant".to_owned(), "answer 7".to_owned()),
                ("user".to_owned(), "question 8".to_owned())
            ]
        ]
        .concat()
    );
}

fn after<'a>(screen: &'a str, marker: &str) -> &'a str {
    let start = screen
        .rfind(marker)
        .unwrap_or_else(|| panic!("expected {marker:?} on screen:\n{screen}"));
    &screen[start..]
}

#[test]
fn the_skills_discovery_warning_shows_again_in_a_new_and_in_a_resumed_conversation() {
    const WARNING: &str = "skill discovery warning";
    let server = FakeServer::start(
        (1..=5).map(|turn| Reply::sse(&chat_text_events(&[&format!("answer {turn}")]))),
    );
    let home = Home::new(&server.base_url());
    let broken = home.workspace.join("skills/broken");
    fs::create_dir_all(&broken).expect("create the skill directory");
    fs::write(broken.join("SKILL.md"), "---\ndescription: nameless\n---\n")
        .expect("write the skill");
    let session = home.shell(&[], WELCOME);
    session.send(b"question 1\r");
    let screen = wait(&session, "answer 1");
    assert!(after(&screen, "┃ question 1").contains(WARNING), "{screen}");
    session.send(b"question 2\r");
    let screen = wait(&session, "answer 2");
    assert!(
        !after(&screen, "┃ question 2").contains(WARNING),
        "{screen}"
    );
    session.send(b"/new\r");
    session.send(b"question 3\r");
    let screen = wait(&session, "answer 3");
    assert!(!screen.contains("answer 2"), "{screen}");
    assert!(after(&screen, "┃ question 3").contains(WARNING), "{screen}");
    session.send(b"question 4\r");
    let screen = wait(&session, "answer 4");
    assert!(
        !after(&screen, "┃ question 4").contains(WARNING),
        "{screen}"
    );
    exit(session);

    let session = home.shell(&["-c"], "session resumed");
    let screen = wait(&session, "answer 4");
    assert!(!screen.contains(WARNING), "{screen}");
    session.send(b"question 5\r");
    let screen = wait(&session, "answer 5");
    assert!(after(&screen, "┃ question 5").contains(WARNING), "{screen}");
    exit(session);
}
