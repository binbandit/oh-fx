use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ofx_testkit::{
    FakeServer, PtySession, RecordedRequest, RefusedPort, Reply, chat_text_events,
    chat_tool_call_events,
};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);
const WELCOME: &str = "Run /help for commands";
const CANCELLATION: &str = "■ Cancelled · What can oh-fx do differently?";
const STEERING_OPEN: &str = "<user_steering>\nApply this live user update to the current task. Continue working unless the user asks you to stop, the task is complete, or a genuine blocker prevents progress.\n\n";

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

    fn spawn_sized(&self, args: &[&str], rows: u16) -> PtySession {
        PtySession::spawn(self.command_in(&self.workspace, args), rows, 100)
            .expect("spawn oh-fx in a pty")
    }

    fn copy_session(&self, template: &str, id: &str, title: &str, updated_at_ms: u64) {
        let source = self.sessions().join(template);
        let target = self.sessions().join(id);
        fs::create_dir(&target).expect("create the copied session");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))
            .expect("make the copied session private");
        for entry in fs::read_dir(&source).expect("list the template session") {
            let name = entry.expect("a template entry").file_name();
            fs::copy(source.join(&name), target.join(&name)).expect("copy a session file");
        }
        let mut metadata = self.metadata(template);
        metadata["id"] = json!(id);
        metadata["title"] = json!(title);
        metadata["updated_at_ms"] = json!(updated_at_ms);
        fs::write(target.join("session.json"), metadata.to_string())
            .expect("rewrite the copied session.json");
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
            .map(|entry| entry.expect("a session entry"))
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.file_name().into_string().expect("a UTF-8 session id"))
            .filter(|id| ofx_session::is_valid_session_id(id))
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

fn without_steering_wrapper(mut chat: Vec<(String, String)>) -> Vec<(String, String)> {
    for (_, text) in &mut chat {
        if let Some(inner) = text
            .strip_prefix(STEERING_OPEN)
            .and_then(|rest| rest.strip_suffix("\n</user_steering>"))
        {
            *text = inner.to_owned();
        }
    }
    chat
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

fn wait_for_window_title(session: &PtySession, title: &str) {
    let sequence = format!("\x1b]2;{title}\x07");
    let deadline = std::time::Instant::now() + WAIT;
    while !String::from_utf8_lossy(&session.output()).contains(&sequence) {
        assert!(
            std::time::Instant::now() < deadline,
            "expected the window title {title:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_renamed_session_resumes_under_its_new_title() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Noted."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first question\r");
    wait(&session, "Noted.");
    wait_for_window_title(&session, "first question");
    session.send(b"/rename   Release prep \r");
    wait(&session, "* session: renamed to \"Release prep\"");
    wait_for_window_title(&session, "Release prep");
    exit(session);
    let id = home.only_session();
    assert_eq!(home.metadata(&id)["title"], "Release prep");
    let session = home.shell(&["-c"], "session resumed: Release prep");
    wait_for_window_title(&session, "Release prep");
    exit(session);
    assert_eq!(home.session_ids(), [id]);
}

#[test]
fn a_prompt_queued_behind_a_deferred_clear_names_the_fresh_sessions_language() {
    let stubborn = json!({"request": {
        "action": "run",
        "command": "trap '' TERM; touch ready; exec /bin/sleep 30",
        "profile": "clean"
    }})
    .to_string();
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events("call-1", "shell", &stubborn)),
        Reply::sse(&chat_text_events(&["Готово."])),
    ]);
    let home = Home::new(&server.base_url());
    let mut command = home.command_in(&home.workspace, &[]);
    command.env("OH_FX_PERMISSION_MODE", "full-access");
    let session = PtySession::spawn(command, 30, 100).expect("spawn oh-fx in a pty");
    wait(&session, "full access · model-a");
    session.send(b"keep going\r");
    session
        .wait_for(WAIT, |_| home.workspace.join("ready").exists())
        .unwrap_or_else(|screen| panic!("the command never started:\n{screen}"));
    session.send("/clear\rОткрой файл\r".as_bytes());
    wait(&session, "Готово.");
    exit(session);
    let fresh = session_with_prompt(&home, &home.session_ids(), "Открой файл");
    assert_eq!(home.metadata(&fresh)["conversation_language"], "und-Cyrl");
}

#[test]
fn the_shell_saves_the_conversation_language_of_its_prompts() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Lu."])),
        Reply::sse(&chat_text_events(&["Gelesen."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send("Открой страницу\r".as_bytes());
    wait(&session, "Lu.");
    let id = home.only_session();
    assert_eq!(home.metadata(&id)["conversation_language"], "und-Cyrl");
    session.send(b"42\r");
    wait(&session, "Gelesen.");
    exit(session);
    assert_eq!(home.metadata(&id)["conversation_language"], "und-Cyrl");
}

#[test]
fn an_added_directory_that_cannot_be_used_ends_the_launch_before_the_shell_starts() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url());
    fails_with(
        home.spawn(&["--add-dir", "../missing"]),
        "oh-fx: PathNotFound",
    );
    fails_with(home.spawn(&["--add-dir=."]), "oh-fx: PrimaryDirectory");
    assert!(server.requests().is_empty());
}

#[test]
fn an_added_directory_is_named_to_the_model_in_the_shell() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Noted."]))]);
    let home = Home::new(&server.base_url());
    let shared = home.root.join("shared");
    fs::create_dir(&shared).expect("create the added directory");
    let session = home.shell(&["--add-dir", "../shared"], WELCOME);
    session.send(b"hello\r");
    wait(&session, "Noted.");
    exit(session);
    let shared = fs::canonicalize(&shared).expect("canonicalize the added directory");
    let messages = server.requests()[0].json()["messages"].clone();
    let system: Vec<&str> = messages
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "system")
        .filter_map(|message| message["content"].as_str())
        .collect();
    let note = format!(
        "Runtime context: the following additional directories are access-authorized for this run. Relative paths still resolve from the primary workspace. These directories do not contribute AGENTS.md or other project instructions.\n- {}\n",
        shared.display()
    );
    let position = system
        .iter()
        .position(|text| *text == note)
        .unwrap_or_else(|| panic!("{system:#?}"));
    assert!(system[position + 1].starts_with("Runtime context: permission mode is auto."));
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
fn approval_feedback_is_saved_on_its_result_and_resumed_after_it() {
    let read = chat_tool_call_events("call-1", "read_file", r#"{"path":"../notes.txt"}"#);
    let server = FakeServer::start([
        Reply::sse(&read),
        Reply::sse(&chat_text_events(&["Summarized."])),
        Reply::sse(&chat_text_events(&["Again."])),
    ]);
    let home = Home::new(&server.base_url());
    fs::write(home.root.join("notes.txt"), "outside notes\n").expect("write the outside file");
    let session = home.shell(&[], WELCOME);
    session.send(b"read the notes\r");
    wait(&session, "Permission needed · Choose one");
    session.send(b"\t");
    wait(&session, "1. Yes, and tell oh-fx what to do next");
    session.send(b"then summarize them");
    wait(&session, "1. Yes, then summarize them");
    std::thread::sleep(Duration::from_millis(700));
    session.send(b"\r");
    wait(&session, "Summarized.");
    exit(session);
    let id = home.only_session();
    let frames = home.frames(&id);
    let result = frames
        .iter()
        .find_map(|frame| frame["event"].get("tool_result"))
        .expect("a saved tool result");
    assert_eq!(
        result["permission_feedback"],
        json!(["then summarize them"])
    );

    let session = home.shell(&["-c"], "session resumed: read the notes");
    let screen = wait(&session, "Summarized.");
    assert!(screen.contains("┃ then summarize them"), "{screen}");
    session.send(b"again\r");
    wait(&session, "Again.");
    exit(session);
    let messages = chat(&server.requests()[2]);
    let tool = messages
        .iter()
        .position(|(role, _)| role == "tool")
        .expect("the tool result");
    assert!(messages[tool].1.contains("outside notes"), "{messages:?}");
    assert_eq!(
        messages[tool + 1],
        ("user".to_owned(), "then summarize them".to_owned())
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
    let session = home.shell(&["resume", &id], "session resumed: saved turn");
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
    let live = without_steering_wrapper(chat(&server.requests()[5]));
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
fn session_discovery_waits_for_publication_before_touching_tool_results() {
    let home = Home::new("http://127.0.0.1:1");
    let staging = home.sessions().join("creating+0011");
    fs::create_dir_all(&staging).expect("prepare the unpublished session");
    assert!(
        home.session_ids().is_empty(),
        "an unpublished session became visible"
    );
    let published = home.sessions().join("published-session");
    fs::rename(&staging, &published).expect("publish the prepared session");
    let id = home.only_session();
    assert_eq!(id, "published-session");
    let blocker = home.sessions().join(&id).join("tool-results");
    fs::write(&blocker, "blocked").expect("block the published tool results");
    fs::remove_file(&blocker).expect("unblock the published tool results");
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
    let live = without_steering_wrapper(live);

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

#[test]
fn file_mentions_complete_in_new_and_resumed_sessions_and_are_saved_as_typed() {
    let server = FakeServer::start(
        (1..=3).map(|turn| Reply::sse(&chat_text_events(&[&format!("answer {turn}")]))),
    );
    let home = Home::new(&server.base_url());
    fs::create_dir_all(home.workspace.join("docs")).expect("create docs");
    fs::create_dir_all(home.workspace.join("src")).expect("create src");
    fs::write(home.workspace.join("docs/my notes.md"), "").expect("write the notes");
    fs::write(home.workspace.join("src/old.rs"), "").expect("write the source");
    let session = home.shell(&[], WELCOME);
    session.send(b"read @notes");
    wait(&session, "docs/my notes.md");
    session.send(b"\r");
    wait(&session, "┃ read @\"docs/my notes.md\"");
    session.send(b"\r");
    wait(&session, "answer 1");
    session.send(b"/new\r");
    session
        .wait_for(WAIT, |screen| !screen.contains("answer 1"))
        .unwrap_or_else(|screen| panic!("the old conversation stayed on screen:\n{screen}"));
    session.send(b"next @old");
    wait(&session, "src/old.rs");
    session.send(b"\r");
    wait(&session, "┃ next @src/old.rs");
    session.send(b"\r");
    wait(&session, "answer 2");
    exit(session);
    let remembered = home.remembered().expect("a remembered session");
    assert_eq!(
        home.frames(&remembered)[0]["event"]["user"]["text"],
        "next @src/old.rs"
    );

    let session = home.shell(&["-c"], "session resumed");
    let screen = wait(&session, "answer 2");
    assert!(screen.contains("┃ next @src/old.rs"), "{screen}");
    assert!(!screen.contains("my notes"), "{screen}");
    session.send(b"last @notes");
    wait(&session, "docs/my notes.md");
    session.send(b"\r");
    wait(&session, "┃ last @\"docs/my notes.md\"");
    session.send(b"\r");
    wait(&session, "answer 3");
    exit(session);
    assert_eq!(
        chat(&server.requests()[2]),
        [
            turn("next @src/old.rs", "answer 2"),
            vec![("user".to_owned(), "last @\"docs/my notes.md\"".to_owned())]
        ]
        .concat()
    );
}

const DEPTH_QUESTION: &str = r#"{"questions":[{"question":"Which depth?","options":[{"label":"Thorough"},{"label":"Fast"}]}]}"#;

fn appears_in_order(screen: &str, pieces: &[&str]) -> bool {
    let mut rest = screen;
    pieces.iter().all(|piece| match rest.find(piece) {
        Some(at) => {
            rest = &rest[at + piece.len()..];
            true
        }
        None => false,
    })
}

#[test]
fn a_resumed_shell_replays_answered_questions_and_asks_new_ones() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call-1",
            "ask_user_question",
            DEPTH_QUESTION,
        )),
        Reply::sse(&chat_text_events(&["Going fast."])),
        Reply::sse(&chat_tool_call_events(
            "call-2",
            "ask_user_question",
            DEPTH_QUESTION,
        )),
        Reply::sse(&chat_text_events(&["Now thorough."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"pick for me\r");
    wait(&session, "Which depth?");
    session.send(b"2");
    wait(&session, "Going fast.");
    exit(session);

    let session = home.shell(&["-c"], "session resumed: pick for me");
    let screen = wait(&session, "Going fast.");
    assert!(screen.contains("  1) Which depth?\n     Fast"), "{screen}");
    assert!(
        appears_in_order(
            &screen,
            &["┃ pick for me", "  1) Which depth?", "Going fast."]
        ),
        "{screen}"
    );
    session.send(b"ask again\r");
    wait(&session, "1–3 choose now");
    session.send(b"1");
    wait(&session, "Now thorough.");
    exit(session);
    let answers: Vec<String> = chat(&server.requests()[3])
        .into_iter()
        .filter(|(role, _)| role == "tool")
        .map(|(_, content)| content)
        .collect();
    assert_eq!(
        answers,
        [
            r#"[{"question":"Which depth?","answer":"Fast"}]"#,
            r#"[{"question":"Which depth?","answer":"Thorough"}]"#,
        ]
    );
}

const PICKER_HEADER: &str = "Sessions 1  [Current workspace]  All workspaces";
const BUSY_IN_PICKER: &str =
    "This session is open in another oh-fx. Close it there, then press enter to retry.";

fn session_with_prompt(home: &Home, ids: &[String], prompt: &str) -> String {
    ids.iter()
        .find(|id| home.frames(id)[0]["event"]["user"]["text"] == prompt)
        .unwrap_or_else(|| panic!("a session that starts with {prompt:?}"))
        .clone()
}

#[test]
fn resume_lists_this_workspace_and_switches_the_shell_to_the_chosen_session() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Answer one."])),
        Reply::sse(&chat_text_events(&["Answer two."])),
        Reply::sse(&chat_text_events(&["Back in one."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"topic one\r");
    wait(&session, "Answer one.");
    session.send(b"/new\r");
    session.send(b"topic two\r");
    wait(&session, "Answer two.");
    session.send(b"/resume\r");
    let screen = wait(&session, PICKER_HEADER);
    assert!(
        screen.contains("  topic one    workspace · now · 1 turn"),
        "{screen}"
    );
    session.send(b"\r");
    let screen = wait(&session, "session resumed: topic one");
    assert!(!screen.contains("Answer two."), "{screen}");
    assert!(!screen.contains(PICKER_HEADER), "{screen}");
    assert!(
        screen.contains("* session resumed: topic one\n\n┃ topic one\n\n  Answer one."),
        "{screen}"
    );
    session.send(b"more one\r");
    wait(&session, "Back in one.");
    exit(session);
    let ids = home.session_ids();
    assert_eq!(ids.len(), 2, "{ids:?}");
    let first = session_with_prompt(&home, &ids, "topic one");
    let second = session_with_prompt(&home, &ids, "topic two");
    assert_eq!(home.remembered(), Some(first.clone()));
    assert_eq!(home.metadata(&first)["title"], "topic one");
    assert_eq!(home.frames(&first).len(), 6);
    assert_eq!(home.frames(&second).len(), 3);
    assert_eq!(
        chat(&server.requests()[2]),
        [
            turn("topic one", "Answer one."),
            vec![("user".to_owned(), "more one".to_owned())]
        ]
        .concat()
    );
}

#[test]
fn a_picked_session_leaves_nothing_for_undo_from_the_session_before_it() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Answer one."])),
        Reply::sse(&chat_tool_call_events(
            "call-1",
            "write_file",
            &json!({"path": "notes.md", "content": "changed\n"}).to_string(),
        )),
        Reply::sse(&chat_text_events(&["Wrote it."])),
    ]);
    let home = Home::new(&server.base_url());
    let notes = home.workspace.join("notes.md");
    fs::write(&notes, "original\n").expect("write notes.md");
    let session = home.shell(&[], WELCOME);
    session.send(b"topic one\r");
    wait(&session, "Answer one.");
    session.send(b"/new\r");
    session.send(b"write the notes\r");
    wait(&session, "Wrote it.");
    session.send(b"/resume\r");
    wait(&session, PICKER_HEADER);
    session.send(b"\r");
    wait(&session, "session resumed: topic one");
    session.send(b"/undo\r");
    wait(&session, "* undo: Nothing to undo.");
    assert_eq!(
        fs::read_to_string(&notes).expect("read notes.md"),
        "changed\n"
    );
    exit(session);
}

#[test]
fn a_session_open_in_another_shell_shows_busy_in_the_picker_until_it_closes() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Held."])),
        Reply::sse(&chat_text_events(&["Taken over."])),
    ]);
    let home = Home::new(&server.base_url());
    let holder = home.shell(&[], WELCOME);
    holder.send(b"held topic\r");
    wait(&holder, "Held.");
    let session = home.shell(&[], WELCOME);
    session.send(b"/resume\r");
    wait(&session, "  held topic    workspace · now · 1 turn");
    session.send(b"\r");
    let screen = wait(&session, BUSY_IN_PICKER);
    assert!(screen.contains(PICKER_HEADER), "{screen}");
    exit(holder);
    session.send(b"\r");
    wait(&session, "session resumed: held topic");
    session.send(b"after the holder\r");
    wait(&session, "Taken over.");
    exit(session);
    let id = home.only_session();
    assert_eq!(home.frames(&id).len(), 6);
}

#[test]
fn the_picker_filters_by_typed_text_and_reaches_sessions_of_other_workspaces() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Alpha."])),
        Reply::sse(&chat_text_events(&["Beta."])),
        Reply::sse(&chat_text_events(&["Moved."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"alpha work\r");
    wait(&session, "Alpha.");
    exit(session);
    let alpha = home.only_session();
    let other = home.root.join("other");
    fs::create_dir_all(&other).expect("create another workspace");
    let session = home.spawn_in(&other, &[]);
    wait(&session, WELCOME);
    session.send(b"beta work\r");
    wait(&session, "Beta.");
    session.send(b"/resume\r");
    let screen = wait(&session, "No sessions found.");
    assert!(
        screen.contains("Sessions 0  [Current workspace]  All workspaces"),
        "{screen}"
    );
    session.send(b"\x1b[Z");
    let screen = wait(&session, "  alpha work    workspace · now · 1 turn");
    assert!(
        screen.contains("Sessions 1  Current workspace  [All workspaces]"),
        "{screen}"
    );
    session.send(b"\x1b");
    session
        .wait_for(WAIT, |screen| !screen.contains("[All workspaces]"))
        .unwrap_or_else(|screen| panic!("the picker stays open:\n{screen}"));
    session.send(b"\x1b[114;9u");
    wait(&session, "Sessions 1  Current workspace  [All workspaces]");
    session.send(b"zzz");
    wait(&session, "Sessions 0  Current workspace  [All workspaces]");
    session.send(b"\x7f\x7f\x7fALP");
    wait(&session, "Sessions 1  Current workspace  [All workspaces]");
    session.send(b"\r");
    wait(&session, "session resumed: alpha work");
    session.send(b"here now\r");
    wait(&session, "Moved.");
    exit(session);
    let other = fs::canonicalize(&other).expect("canonicalize the other workspace");
    assert_eq!(
        home.metadata(&alpha)["workspace_root"],
        other.to_str().expect("a UTF-8 path")
    );
}

#[test]
fn picking_at_launch_resumes_the_choice_or_starts_fresh_when_closed() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["One."])),
        Reply::sse(&chat_text_events(&["Two."])),
        Reply::sse(&chat_text_events(&["Fresh."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first\r");
    wait(&session, "One.");
    exit(session);
    let first = home.only_session();
    let session = home.spawn(&["-r"]);
    let screen = wait(&session, PICKER_HEADER);
    assert!(!screen.contains(WELCOME), "{screen}");
    session.send(b"\r");
    wait(&session, "session resumed: first");
    session.send(b"second\r");
    wait(&session, "Two.");
    exit(session);
    assert_eq!(home.session_ids(), std::slice::from_ref(&first));
    let session = home.spawn(&["-r"]);
    wait(&session, PICKER_HEADER);
    session.send(b"\x1b");
    session
        .wait_for(WAIT, |screen| !screen.contains(PICKER_HEADER))
        .unwrap_or_else(|screen| panic!("the picker stays open:\n{screen}"));
    session.send(b"third\r");
    wait(&session, "Fresh.");
    exit(session);
    assert_eq!(home.session_ids().len(), 2);
    assert_eq!(
        chat(&server.requests()[2]),
        [("user".to_owned(), "third".to_owned())]
    );
    let session = home.spawn(&["-r"]);
    wait(&session, "Sessions 2  [Current workspace]  All workspaces");
    exit(session);
    assert_eq!(home.session_ids().len(), 2);
}

#[test]
fn resume_waits_for_a_running_response() {
    let server = FakeServer::start([Reply::held_sse(
        &chat_text_events(&["Still going.\nMore.\n", "never"])[..2],
    )]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"long task\r");
    wait(&session, "Still going.");
    session.send(b"/resume\r");
    let screen = wait(
        &session,
        "session: resume is unavailable until the response finishes",
    );
    assert!(!screen.contains("[Current workspace]"), "{screen}");
    session.send(b"\x1b[114;9u");
    session
        .wait_for(WAIT, |screen| {
            screen
                .matches("resume is unavailable until the response finishes")
                .count()
                == 2
        })
        .unwrap_or_else(|screen| panic!("Super+R was not refused:\n{screen}"));
    session.send(b"\x03");
    wait(&session, CANCELLATION);
    exit(session);
}

#[test]
fn more_sessions_load_as_the_selection_reaches_the_end_of_a_page() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Seeded."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"seed\r");
    wait(&session, "Seeded.");
    exit(session);
    let seed = home.only_session();
    let copied_at_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after the epoch")
            .as_millis(),
    )
    .expect("a millisecond clock that fits");
    for index in 0..12_u64 {
        home.copy_session(
            &seed,
            &format!("copy-{index:02}"),
            &format!("copy {index:02}"),
            copied_at_ms + 1 + index,
        );
    }
    let session = home.spawn_sized(&["-r"], 14);
    let screen = wait(&session, "Sessions 10  [Current workspace]  All workspaces");
    assert!(screen.contains("  ↓ Load more"), "{screen}");
    assert!(screen.contains("  copy 11 "), "{screen}");
    for _ in 0..9 {
        session.send(b"\x1b[B");
    }
    let screen = wait(&session, "Sessions 13  [Current workspace]  All workspaces");
    assert!(!screen.contains("Load more"), "{screen}");
    session.send(b"\x1b[B\x1b[B\x1b[B\x1b[B");
    let screen = wait(&session, "  seed ");
    assert!(!screen.contains("  copy 11 "), "{screen}");
    session.send(b"\r");
    wait(&session, "session resumed: seed");
    exit(session);
}

#[test]
fn a_picked_session_brings_back_its_model_and_one_from_another_provider_is_refused() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["On b."])),
        Reply::sse(&chat_text_events(&["Elsewhere."])),
        Reply::sse(&chat_text_events(&["Back on b."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"/model vendor/model-b\r");
    wait(&session, "auto · model-b");
    session.send(b"use b\r");
    wait(&session, "On b.");
    exit(session);
    let on_b = home.only_session();
    let mut renamed = settings(&server.base_url());
    renamed["providers"]["other"] = renamed["providers"]["local"].clone();
    renamed["provider"] = json!("other");
    fs::write(
        home.root.join("config/oh-fx/settings.json"),
        renamed.to_string(),
    )
    .expect("rewrite settings.json");
    let elsewhere = home.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("create another workspace");
    let session = home.spawn_in(&elsewhere, &[]);
    wait(&session, WELCOME);
    session.send(b"elsewhere\r");
    wait(&session, "Elsewhere.");
    exit(session);
    let other = session_with_prompt(&home, &home.session_ids(), "elsewhere");
    let other_workspace = home.metadata(&other)["workspace_root"].clone();
    renamed["provider"] = json!("local");
    fs::write(
        home.root.join("config/oh-fx/settings.json"),
        renamed.to_string(),
    )
    .expect("restore the provider");
    let session = home.shell(&[], WELCOME);
    session.send(b"/resume\r");
    wait(&session, PICKER_HEADER);
    session.send(b"\x1b[Z");
    wait(&session, "Sessions 2  Current workspace  [All workspaces]");
    session.send(b"\r");
    let screen = wait(&session, "  Unable to resume this session.");
    assert!(
        screen.contains(&format!(
            "This session was saved with another provider. Resume it with oh-fx resume {other}."
        )),
        "{screen}"
    );
    session.send(b"\x1b[B\r");
    wait(&session, "session resumed: use b");
    wait(&session, "auto · model-b");
    session.send(b"again on b\r");
    wait(&session, "Back on b.");
    exit(session);
    let models: Vec<Value> = server
        .requests()
        .iter()
        .map(|request| request.json()["model"].clone())
        .collect();
    assert_eq!(models, ["vendor/model-b", "model-a", "vendor/model-b"]);
    assert_eq!(home.metadata(&on_b)["model"], "vendor/model-b");
    assert_eq!(home.metadata(&other)["provider"]["name"], "other");
    assert_eq!(home.metadata(&other)["workspace_root"], other_workspace);
}

#[test]
fn a_fresh_session_after_a_pick_saves_the_preferences_it_runs_with() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["On b."])),
        Reply::sse(&chat_text_events(&["Fresh on b."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"/model vendor/model-b\r");
    wait(&session, "auto · model-b");
    session.send(b"use b\r");
    wait(&session, "On b.");
    exit(session);
    let on_b = home.only_session();
    fs::write(
        home.root.join("config/oh-fx/settings.json"),
        settings(&server.base_url()).to_string(),
    )
    .expect("configure model-a again");
    let session = home.shell(&[], WELCOME);
    session.send(b"/resume\r");
    wait(&session, PICKER_HEADER);
    session.send(b"\r");
    wait(&session, "session resumed: use b");
    wait(&session, "auto · model-b");
    session.send(b"/new\r");
    session.send(b"fresh start\r");
    wait(&session, "Fresh on b.");
    exit(session);
    let models: Vec<Value> = server
        .requests()
        .iter()
        .map(|request| request.json()["model"].clone())
        .collect();
    assert_eq!(models, ["vendor/model-b", "vendor/model-b"]);
    let fresh = session_with_prompt(&home, &home.session_ids(), "fresh start");
    assert_ne!(fresh, on_b);
    for field in ["model", "effort", "fast_mode"] {
        assert_eq!(
            home.metadata(&fresh)[field],
            home.metadata(&on_b)[field],
            "{field}"
        );
    }
}

#[test]
fn a_launch_model_flag_outlasts_the_model_of_a_picked_session() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["On b."])),
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
    let session = home.spawn(&["--model", "model-a", "-r"]);
    wait(&session, PICKER_HEADER);
    session.send(b"\r");
    let screen = wait(&session, "session resumed: use b");
    assert!(screen.contains("auto · model-a"), "{screen}");
    session.send(b"flagged\r");
    wait(&session, "Flagged a.");
    exit(session);
    let models: Vec<Value> = server
        .requests()
        .iter()
        .map(|request| request.json()["model"].clone())
        .collect();
    assert_eq!(models, ["vendor/model-b", "model-a"]);
    assert_eq!(home.metadata(&id)["model"], "vendor/model-b");
}

fn tool_group(screen: &str) -> Vec<&str> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.starts_with("● "))
        .unwrap_or_else(|| panic!("no tool group on screen:\n{screen}"));
    lines[start..]
        .iter()
        .take_while(|line| !line.trim().is_empty())
        .copied()
        .collect()
}

#[test]
fn a_resumed_shell_replays_its_tool_rows_without_the_unsaved_failure_detail() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call-1",
            "read_file",
            r#"{"path":"notes.md"}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call-2",
            "read_file",
            r#"{"path":"missing.md"}"#,
        )),
        Reply::sse(&chat_text_events(&["Read them."])),
    ]);
    let home = Home::new(&server.base_url());
    fs::write(home.workspace.join("notes.md"), "hello\n").expect("write a note");
    let session = home.shell(&[], WELCOME);
    session.send(b"read the notes\r");
    let live = wait(&session, "Read them.");
    let rows = [
        "● 2 tool calls · 2 read · 1 failed",
        "├ Read notes.md",
        "└ Failed missing.md",
    ];
    assert_eq!(
        tool_group(&live),
        [
            rows[0],
            rows[1],
            "└ Failed missing.md: Path not found: missing.md"
        ],
        "{live}"
    );
    exit(session);

    let session = home.shell(&["-c"], "session resumed: read the notes");
    let resumed = wait(&session, "Read them.");
    assert_eq!(tool_group(&resumed), rows, "{resumed}");
    assert!(
        appears_in_order(&resumed, &["┃ read the notes", rows[0], "Read them."]),
        "{resumed}"
    );
    exit(session);
}

fn saved_call(seq: u64, id: &str, tool: &str, arguments: &Value) -> Vec<u8> {
    frame(
        seq,
        &json!({"tool_call": {"call_id": id, "tool_name": tool, "arguments_json": arguments.to_string()}}),
    )
}

fn saved_result(seq: u64, id: &str, tool: &str, status: &str, output: &str) -> Vec<u8> {
    frame(
        seq,
        &json!({"tool_result": {
            "call_id": id,
            "tool_name": tool,
            "status": status,
            "artifact_ref": format!("result-{id}.txt"),
            "stored_bytes": output.len(),
            "completeness": "complete",
            "preview": output,
        }}),
    )
}

#[test]
fn a_resumed_shell_labels_saved_tool_results_and_drops_the_call_an_interruption_left_open() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Ready."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first\r");
    wait(&session, "Ready.");
    exit(session);
    let id = home.only_session();
    let denied = json!({"error": {
        "type": "tool_permission_denied",
        "tool_name": "read_file",
        "message": "denied",
        "reason": "user_denied",
        "denied": true,
        "suggestion": "ask",
    }})
    .to_string();
    let deferred = "Scoped project instructions were added before execution. Review them and reissue this tool call if it is still appropriate.";
    let question =
        json!({"questions": [{"question": "Which?", "options": [{"label": "A"}, {"label": "B"}]}]});
    home.append(
        &id,
        &[
            frame(4, &json!({"user": {"text": "check the files"}})),
            saved_call(5, "c1", "read_file", &json!({"path": "a.md"})),
            saved_call(6, "c2", "read_file", &json!({"path": "b.md"})),
            saved_call(7, "c3", "read_file", &json!({"path": "c.md"})),
            saved_result(8, "c1", "read_file", "failure", &denied),
            saved_result(9, "c2", "read_file", "failure", "Not executed"),
            saved_result(10, "c3", "read_file", "failure", deferred),
            saved_call(11, "c4", "ask_user_question", &question),
            saved_result(
                12,
                "c4",
                "ask_user_question",
                "success",
                "(user cancelled the question)",
            ),
            frame(13, &json!({"assistant": {"text": "Checked."}})),
            frame(14, &json!({"turn_completed": {}})),
            frame(15, &json!({"user": {"text": "stop now"}})),
            saved_call(16, "c5", "read_file", &json!({"path": "d.md"})),
            frame(17, &json!({"interrupted": {"reason": "cancelled"}})),
        ]
        .concat(),
    );

    let session = home.shell(&["-c"], "session resumed: first");
    let screen = wait(&session, CANCELLATION);
    assert!(
        appears_in_order(
            &screen,
            &[
                "┃ check the files",
                "├ Denied a.md",
                "├ Not executed b.md",
                "├ Reading project instructions before continuing: c.md",
                "└ Asked",
                "Checked.",
                "┃ stop now",
                CANCELLATION,
            ]
        ),
        "{screen}"
    );
    assert!(!screen.contains("d.md"), "{screen}");
    assert!(!screen.contains("Tool cancelled"), "{screen}");
    exit(session);
}

fn saved_command_result(seq: u64, id: &str, presentation: &Value) -> Vec<u8> {
    frame(
        seq,
        &json!({"tool_result": {
            "call_id": id,
            "tool_name": "shell",
            "status": "failure",
            "artifact_ref": format!("result-{id}.txt"),
            "stored_bytes": 0,
            "completeness": "complete",
            "preview": "",
            "command_process_presentation": presentation,
        }}),
    )
}

#[test]
fn a_resumed_shell_labels_a_captured_command_with_its_saved_process_outcome() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Ready."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first\r");
    wait(&session, "Ready.");
    exit(session);
    let id = home.only_session();
    let run = |command: &str| json!({"action": "run", "command": command});
    home.append(
        &id,
        &[
            frame(4, &json!({"user": {"text": "run the checks"}})),
            saved_call(5, "c1", "shell", &run("make check")),
            saved_call(6, "c2", "shell", &run("sleep 60")),
            saved_call(7, "c3", "shell", &run("kill -9 $$")),
            saved_call(
                8,
                "c4",
                "shell",
                &json!({"action": "run", "command": "make lint", "tty": true}),
            ),
            saved_command_result(9, "c1", &json!({"exit_code": 2})),
            saved_command_result(10, "c2", &json!({"timed_out": {}})),
            saved_command_result(11, "c3", &json!({"signal": 9})),
            saved_command_result(12, "c4", &json!({"exit_code": 2})),
            frame(13, &json!({"assistant": {"text": "Checked."}})),
            frame(14, &json!({"turn_completed": {}})),
        ]
        .concat(),
    );

    let session = home.shell(&["-c"], "session resumed: first");
    let screen = wait(&session, "Checked.");
    assert!(
        appears_in_order(
            &screen,
            &[
                "┃ run the checks",
                "├ Exited 2 make check",
                "├ Timed out sleep 60",
                "├ Signaled 9 kill -9 $$",
                "└ Failed make lint",
                "Checked.",
            ]
        ),
        "{screen}"
    );
    exit(session);
}

#[test]
fn a_resumed_skill_row_names_the_skill_its_saved_result_loaded() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Ready."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first\r");
    wait(&session, "Ready.");
    exit(session);
    let id = home.only_session();
    let loaded = "<skill_content name=\"workflow\" location=\"/skills/workflow\" resource=\"SKILL.md\" complete=\"true\">\nFollow the workflow.\n</skill_content>";
    home.append(
        &id,
        &[
            frame(4, &json!({"user": {"text": "use the skill"}})),
            saved_call(5, "c1", "skill", &json!({"location": "/skills/workflow"})),
            saved_result(6, "c1", "skill", "success", loaded),
            saved_call(7, "c2", "skill", &json!({"location": "/skills/missing"})),
            saved_result(8, "c2", "skill", "failure", "skill failed: missing"),
            frame(9, &json!({"assistant": {"text": "Used it."}})),
            frame(10, &json!({"turn_completed": {}})),
        ]
        .concat(),
    );

    let session = home.shell(&["-c"], "session resumed: first");
    let screen = wait(&session, "Used it.");
    assert!(
        appears_in_order(
            &screen,
            &[
                "┃ use the skill",
                "├ Loaded skill workflow",
                "└ Failed skill",
                "Used it.",
            ]
        ),
        "{screen}"
    );
    exit(session);
}

#[test]
fn a_resumed_provider_search_keeps_its_search_row() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Ready."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first\r");
    wait(&session, "Ready.");
    exit(session);
    let id = home.only_session();
    let found = r#"{"results":[]}"#;
    let arguments = json!({"query": "zig news", "allowed_domains": ["ziglang.org"]});
    home.append(
        &id,
        &[
            frame(4, &json!({"user": {"text": "search the web"}})),
            frame(
                5,
                &json!({"tool_call": {
                    "call_id": "s1",
                    "tool_name": "exa_search",
                    "arguments_json": arguments.to_string(),
                    "provider_result": found,
                    "provenance": "provider_executed",
                }}),
            ),
            frame(
                6,
                &json!({"tool_call": {
                    "call_id": "s2",
                    "tool_name": "parallel_search",
                    "arguments_json": "[]",
                    "argument_integrity": "non_object_json",
                    "provider_result": found,
                    "provenance": "provider_executed",
                }}),
            ),
            saved_result(7, "s1", "exa_search", "success", found),
            saved_result(8, "s2", "parallel_search", "success", found),
            frame(9, &json!({"assistant": {"text": "Searched."}})),
            frame(10, &json!({"turn_completed": {}})),
        ]
        .concat(),
    );

    let session = home.shell(&["-c"], "session resumed: first");
    let screen = wait(&session, "Searched.");
    assert!(
        appears_in_order(
            &screen,
            &[
                "┃ search the web",
                "├ Searched zig news | allowed: ziglang.org",
                "└ Completed tool call",
                "Searched.",
            ]
        ),
        "{screen}"
    );
    exit(session);
}

#[test]
fn a_resumed_subagent_row_reads_its_outcome_from_the_whole_saved_result() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Ready."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first\r");
    wait(&session, "Ready.");
    exit(session);
    let id = home.only_session();
    let partial = "partial work ".repeat(600);
    let output =
        json!({"ok": false, "result": partial, "error_code": "child_interrupted"}).to_string();
    assert!(output.len() > 4096);
    let handle = "result-subagent-0011223344556677-8899aabbccddeeff.txt";
    let results = home.sessions().join(&id).join("tool-results");
    fs::create_dir(&results).expect("create the result store");
    fs::set_permissions(&results, fs::Permissions::from_mode(0o700))
        .expect("make the result store private");
    fs::write(results.join(handle), &output).expect("store the whole result");
    fs::set_permissions(results.join(handle), fs::Permissions::from_mode(0o600))
        .expect("make the result private");
    let preview = &output[..4096];
    home.append(
        &id,
        &[
            frame(4, &json!({"user": {"text": "delegate it"}})),
            saved_call(
                5,
                "c1",
                "subagent",
                &json!({"request": {"action": "run", "task": "inspect auth"}}),
            ),
            frame(
                6,
                &json!({"tool_result": {
                    "call_id": "c1",
                    "tool_name": "subagent",
                    "status": "failure",
                    "artifact_ref": handle,
                    "stored_bytes": output.len(),
                    "completeness": "complete",
                    "preview": preview,
                }}),
            ),
            frame(7, &json!({"assistant": {"text": "Delegated."}})),
            frame(8, &json!({"turn_completed": {}})),
        ]
        .concat(),
    );

    let session = home.shell(&["-c"], "session resumed: first");
    let screen = wait(&session, "Delegated.");
    assert!(
        appears_in_order(
            &screen,
            &[
                "┃ delegate it",
                "Subagent interrupted · inspect auth",
                "Delegated."
            ]
        ),
        "{screen}"
    );
    exit(session);
}

const CONFIGURED_IDENTITY: &str =
    "40122b758656199048961e6e8369383c25ebcdeddced75b64ad736e527014da8";

fn pause_a_response(home: &Home, id: &str) {
    pause_a_response_under(home, id, CONFIGURED_IDENTITY);
}

fn pause_a_response_under(home: &Home, id: &str, credential: &str) {
    save_paused(home, id, credential, |_| {});
}

fn save_paused(home: &Home, id: &str, credential: &str, edit: impl FnOnce(&mut Value)) {
    let mut checkpoint = json!({
        "version": 2,
        "turn_id": 2,
        "user": {"text": "fix the build", "images": []},
        "assistant_source": "",
        "execution": {
            "schema_version": 10,
            "tool_steps": [],
            "files": [],
            "steering": [],
            "turn_summary": null
        },
        "cause": "rate_limited",
        "action": "retrying_request",
        "tool_state": "none",
        "authority": {
            "provider": home.metadata(id)["provider"],
            "model": "model-a",
            "credential_source": "configured",
            "credential_identity": credential
        },
        "requested_fast_mode": false,
        "fast_mode": false,
        "max_provider_attempts": 10,
        "consumed_provider_attempts": 1,
        "outstanding_reservation": false
    });
    edit(&mut checkpoint);
    let seq = home.frames(id).len();
    fs::write(
        home.sessions().join(id).join("recovery.json"),
        format!("{{\"conversation_seq\":{seq},\"checkpoint\":{checkpoint}}}\n"),
    )
    .expect("write recovery.json");
}

#[test]
fn a_resumed_shell_continues_a_paused_response_on_its_own() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["First answer."])),
        Reply::sse(&chat_text_events(&["Build fixed."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first question\r");
    wait(&session, "First answer.");
    exit(session);
    let id = home.only_session();
    pause_a_response(&home, &id);

    let session = home.shell(&["-c"], "session resumed: first question");
    let screen = wait(&session, "Build fixed.");
    assert!(
        appears_in_order(
            &screen,
            &[
                "First answer.",
                "fix the build",
                "model response recovery paused and continues automatically",
                "Build fixed."
            ]
        ),
        "{screen}"
    );
    exit(session);
    assert_eq!(
        chat(&server.requests()[1]),
        [
            turn("first question", "First answer."),
            vec![("user".to_owned(), "fix the build".to_owned())]
        ]
        .concat()
    );
    assert!(!home.sessions().join(&id).join("recovery.json").exists());
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
fn a_resumed_shell_restarts_a_paused_reply_without_showing_it_twice() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["First answer."])),
        Reply::sse(&chat_text_events(&["Build fixed."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first question\r");
    wait(&session, "First answer.");
    exit(session);
    let id = home.only_session();
    save_paused(&home, &id, CONFIGURED_IDENTITY, |checkpoint| {
        checkpoint["assistant_source"] = json!("Looking at the build");
        checkpoint["cause"] = json!("network_interrupted");
        checkpoint["action"] = json!("continuing_response");
    });

    let session = home.shell(&["-c"], "session resumed: first question");
    let screen = wait(&session, "Build fixed.");
    assert_eq!(
        screen.matches("Looking at the build").count(),
        1,
        "{screen}"
    );
    assert!(
        appears_in_order(
            &screen,
            &[
                "fix the build",
                "Looking at the build",
                "continues automatically",
                "[Response interrupted. Restarting.]",
                "Build fixed."
            ]
        ),
        "{screen}"
    );
    exit(session);
    let sent = chat(&server.requests()[1]);
    assert!(
        sent.last().is_some_and(|(role, content)| role == "user"
            && content.starts_with("The previous response was interrupted.")),
        "{sent:?}"
    );
}

#[test]
fn a_shell_that_quit_unexpectedly_asks_before_continuing_a_paused_response() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["First answer."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first question\r");
    wait(&session, "First answer.");
    exit(session);
    let id = home.only_session();
    pause_a_response(&home, &id);
    fs::write(
        home.sessions().join(&id).join("owner.live"),
        "{\"pid\":1,\"opened_at_ms\":1}\n",
    )
    .expect("leave an owner marker behind");

    for _ in 0..2 {
        let session = home.shell(&["-c"], "session resumed: first question");
        let screen = wait(&session, "quit unexpectedly");
        assert!(
            appears_in_order(
                &screen,
                &["First answer.", "fix the build", "quit unexpectedly"]
            ),
            "{screen}"
        );
        exit(session);
        assert!(home.sessions().join(&id).join("recovery.asked").exists());
        assert!(home.sessions().join(&id).join("recovery.json").exists());
    }
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_paused_turn_a_compaction_left_open_is_committed_when_the_next_prompt_arrives() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["First answer."])),
        Reply::sse(&chat_text_events(&["Moved on."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first question\r");
    wait(&session, "First answer.");
    exit(session);
    let id = home.only_session();
    home.append(&id, &frame(4, &json!({"user": {"text": "fix the build"}})));
    home.append(
        &id,
        &frame(
            5,
            &json!({"context_checkpoint": {"covers_through_seq": 3, "summary": "<summary>first question answered</summary>"}}),
        ),
    );
    pause_a_response(&home, &id);
    fs::write(
        home.sessions().join(&id).join("owner.live"),
        "{\"pid\":1,\"opened_at_ms\":1}\n",
    )
    .expect("leave an owner marker behind");

    let session = home.shell(&["-c"], "quit unexpectedly");
    assert_eq!(kinds(&home.frames(&id)).len(), 5);
    session.send(b"next question\r");
    wait(&session, "Moved on.");
    exit(session);
    assert_eq!(
        kinds(&home.frames(&id))[4..],
        [
            "context_checkpoint",
            "interrupted",
            "user",
            "assistant",
            "turn_completed"
        ]
    );
    let sent = chat(&server.requests()[1]);
    assert!(
        sent.iter()
            .any(|(role, content)| role == "user" && content == "fix the build"),
        "{sent:?}"
    );
    assert_eq!(
        sent.last(),
        Some(&("user".to_owned(), "next question".to_owned()))
    );
    assert!(!home.sessions().join(&id).join("recovery.json").exists());
}

#[test]
fn a_refused_continuation_leaves_the_paused_turn_for_the_next_prompt_to_commit() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["First answer."])),
        Reply::sse(&chat_text_events(&["Moved on."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first question\r");
    wait(&session, "First answer.");
    exit(session);
    let id = home.only_session();
    home.append(&id, &frame(4, &json!({"user": {"text": "fix the build"}})));
    home.append(
        &id,
        &frame(
            5,
            &json!({"context_checkpoint": {"covers_through_seq": 3, "summary": "<summary>first question answered</summary>"}}),
        ),
    );
    pause_a_response_under(&home, &id, &"ab".repeat(32));

    let session = home.shell(&["-c"], "could not continue automatically");
    assert_eq!(kinds(&home.frames(&id)).len(), 5);
    session.send(b"next question\r");
    wait(&session, "Moved on.");
    exit(session);
    let frames = home.frames(&id);
    assert_eq!(
        kinds(&frames)[4..],
        [
            "context_checkpoint",
            "interrupted",
            "user",
            "assistant",
            "turn_completed"
        ]
    );
    assert_eq!(frames[6]["event"]["user"]["text"], "next question");
    assert_eq!(server.requests().len(), 2);
    let sent = chat(&server.requests()[1]);
    assert!(
        sent.iter()
            .any(|(role, content)| role == "user" && content == "fix the build"),
        "{sent:?}"
    );
    assert_eq!(
        sent.last(),
        Some(&("user".to_owned(), "next question".to_owned()))
    );
    assert!(!home.sessions().join(&id).join("recovery.json").exists());
}

fn turn_summary(duration_ms: u64, input_tokens: u64, output_tokens: u64) -> Value {
    json!({
        "started_at_ms": 1_000,
        "completed_at_ms": 1_000 + duration_ms,
        "thinking_duration_ms": 400,
        "turn_duration_ms": duration_ms,
        "token_progress": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "input_exact": true,
            "output_exact": false
        }
    })
}

#[test]
fn a_resumed_shell_shows_the_turn_summaries_upstream_saved() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Ready."]))]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first\r");
    wait(&session, "Ready.");
    exit(session);
    let id = home.only_session();
    home.append(
        &id,
        &[
            frame(4, &json!({"user": {"text": "fix the build"}})),
            frame(5, &json!({"assistant": {"text": "Fixed it."}})),
            frame(
                6,
                &json!({"turn_completed": {"files": [], "turn_summary": turn_summary(65_000, 1_234, 340)}}),
            ),
            frame(7, &json!({"user": {"text": "stop now"}})),
            frame(
                8,
                &json!({"interrupted": {
                    "reason": "cancelled",
                    "partial_text": null,
                    "command_replay_ref": null,
                    "command_replay_bytes": null,
                    "command_artifact_ref": null,
                    "files": [],
                    "turn_summary": turn_summary(2_000, 50, 0)
                }}),
            ),
        ]
        .concat(),
    );

    let session = home.shell(&["-c"], "session resumed: first");
    let screen = wait(&session, CANCELLATION);
    assert!(
        appears_in_order(
            &screen,
            &[
                "┃ fix the build",
                "Fixed it.",
                "  1m 5s (↑1.2k ↓340)",
                "┃ stop now",
                CANCELLATION,
                "  2s (↑50 ↓0)",
            ]
        ),
        "{screen}"
    );
    exit(session);
}

#[test]
fn a_first_turn_paused_with_escape_is_kept_for_continue_to_resume() {
    let port = RefusedPort::reserve();
    let home = Home::new(&port.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"hi\r");
    wait(&session, " · esc to pause");
    session.send(b"\x1b");
    wait(&session, "recovery paused after 1 attempt");
    exit(session);
    let id = home.only_session();
    assert!(home.sessions().join(&id).join("recovery.json").exists());
    assert!(home.frames(&id).is_empty());

    let session = home.shell(&["-c"], "continues automatically");
    let screen = wait(&session, "waiting for connection");
    assert!(
        appears_in_order(
            &screen,
            &[
                "┃ hi",
                "model response recovery paused and continues automatically",
                "waiting for connection"
            ]
        ),
        "{screen}"
    );
    session.send(b"\x03");
    wait(&session, CANCELLATION);
    exit(session);
}

#[test]
fn a_shell_killed_while_its_first_turn_waits_to_retry_is_reopened_by_continue() {
    let port = RefusedPort::reserve();
    let home = Home::new(&port.base_url());
    let mut session = home.shell(&[], WELCOME);
    session.send(b"hi\r");
    wait(&session, " · esc to pause");
    let remembered = home.sessions().with_file_name("continue");
    let deadline = std::time::Instant::now() + WAIT;
    let named = |entry: fs::DirEntry| {
        let name = entry.file_name();
        name.len() == 64
            && name
                .to_string_lossy()
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    };
    while !fs::read_dir(&remembered).is_ok_and(|entries| entries.filter_map(Result::ok).any(named))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the session was never remembered for -c"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    session.kill().expect("kill oh-fx");
    assert!(session.wait_exit(WAIT).is_some());
    let id = home.only_session();
    assert!(home.sessions().join(&id).join("recovery.json").exists());

    let session = home.shell(&["-c"], "quit unexpectedly");
    let screen = wait(&session, "auto · model-a");
    assert!(
        appears_in_order(&screen, &["┃ hi", "quit unexpectedly"]),
        "{screen}"
    );
    exit(session);
}

fn upstream_shell_call(seq: u64, call_id: &str, command: &str) -> Vec<u8> {
    let arguments = json!({"request": {"action": "run", "command": command}}).to_string();
    frame(
        seq,
        &json!({"tool_call": {
            "call_id": call_id,
            "tool_name": "shell",
            "arguments_json": arguments,
            "argument_integrity": "valid",
            "provisional_id": null,
            "provider_result": null,
            "final_identity": "valid",
            "provenance": "fx_local"
        }}),
    )
}

fn append_fx_command_replay_turns(home: &Home, id: &str, replay: &str) {
    home.append(
        id,
        &[
            frame(
                4,
                &json!({"user": {"text": "list the files", "images": [], "work_id": null}}),
            ),
            upstream_shell_call(5, "call-ls", "ls"),
            frame(
                6,
                &json!({"tool_result": {
                    "call_id": "call-ls",
                    "tool_name": "shell",
                    "status": "success",
                    "artifact_ref": replay,
                    "tool_image_handle": null,
                    "output_bytes": 12,
                    "stored_bytes": 12,
                    "completeness": "complete",
                    "preview": "a.txt\nb.txt\n",
                    "provider_native": false,
                    "created_at_ms": 1,
                    "permission_feedback": [],
                    "committed_file_presentation": null,
                    "command_replay_ref": replay,
                    "command_replay_bytes": 29,
                    "command_process_presentation": {"exit_code": 0},
                    "terminal_action_presentation": null
                }}),
            ),
            frame(7, &json!({"assistant": {"text": "Two files."}})),
            frame(
                8,
                &json!({"turn_completed": {"files": [], "turn_summary": null}}),
            ),
            frame(
                9,
                &json!({"user": {"text": "run the tests", "images": [], "work_id": null}}),
            ),
            upstream_shell_call(10, "call-test", "cargo test"),
            frame(
                11,
                &json!({"interrupted": {
                    "reason": "cancelled",
                    "partial_text": "Running the tests.",
                    "command_replay_ref": replay,
                    "command_replay_bytes": 29,
                    "command_artifact_ref": "fx-command-artifact-1.log",
                    "files": [],
                    "turn_summary": null
                }}),
            ),
        ]
        .concat(),
    );
}

#[test]
fn a_session_holding_the_command_replays_fx_saved_resumes_and_keeps_them() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["Ready."])),
        Reply::sse(&chat_text_events(&["Moved on."])),
    ]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[], WELCOME);
    session.send(b"first\r");
    wait(&session, "Ready.");
    exit(session);
    let id = home.only_session();
    let replay = format!("fx-command-replay-{}.bin", "0123456789abcdef".repeat(4));
    append_fx_command_replay_turns(&home, &id, &replay);

    let session = home.shell(&["-c"], "session resumed: first");
    let screen = wait(&session, CANCELLATION);
    assert!(
        appears_in_order(
            &screen,
            &[
                "┃ list the files",
                "Two files.",
                "┃ run the tests",
                "Running the tests.",
                CANCELLATION,
            ]
        ),
        "{screen}"
    );
    session.send(b"next\r");
    wait(&session, "Moved on.");
    exit(session);
    let messages = chat(&server.requests()[1]);
    let tools: Vec<_> = messages
        .iter()
        .filter(|(role, _)| role == "tool")
        .map(|(_, content)| content.as_str())
        .collect();
    assert_eq!(tools, ["a.txt\nb.txt\n", "aborted by user"]);
    let frames = home.frames(&id);
    assert_eq!(
        frames[5]["event"]["tool_result"]["command_replay_ref"],
        json!(replay)
    );
    assert_eq!(
        frames[10]["event"]["interrupted"]["command_replay_bytes"],
        29
    );
    assert_eq!(
        frames[10]["event"]["interrupted"]["command_artifact_ref"],
        "fx-command-artifact-1.log"
    );
    assert_eq!(
        kinds(&frames)[11..],
        ["user", "assistant", "turn_completed"]
    );
}
