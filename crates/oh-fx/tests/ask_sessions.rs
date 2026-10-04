use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_testkit::{FakeServer, RecordedRequest, Reply, chat_text_events, chat_tool_call_events};
use serde_json::{Value, json};

const KEY: [(&str, &str); 1] = [("PORTKEY_API_KEY", "pk-test-0123456789")];

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new(base_url: &str) -> Self {
        Self::with_settings(&settings(base_url, None))
    }

    fn with_settings(settings: &Value) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        fs::create_dir_all(root.join("config/oh-fx")).expect("create the config directory");
        fs::create_dir_all(root.join("workspace")).expect("create the workspace");
        let home = Self {
            _directory: directory,
            root,
        };
        home.write_settings(settings);
        home
    }

    fn write_settings(&self, settings: &Value) {
        fs::write(
            self.root.join("config/oh-fx/settings.json"),
            settings.to_string(),
        )
        .expect("write settings.json");
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
            .collect();
        ids.sort();
        ids
    }

    fn frames(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(self.sessions().join(id).join("events.jsonl"))
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

    fn command<S: AsRef<OsStr>>(&self, args: &[S]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(KEY)
            .stdin(Stdio::null());
        command
    }

    fn ask<S: AsRef<OsStr>>(&self, args: &[S], environment: &[(&str, &str)]) -> Output {
        self.command(args)
            .envs(environment.iter().copied())
            .output()
            .expect("run oh-fx")
    }

    fn ask_json(&self, args: &[&str], environment: &[(&str, &str)]) -> Value {
        let output = self.ask(&[&["ask", "--json"][..], args].concat(), environment);
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "{args:?}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
}

fn settings(base_url: &str, metadata: Option<Value>) -> Value {
    let mut provider = json!({
        "protocol": "openai-chat-completions",
        "base_url": base_url,
        "auth": {"type": "none"},
        "headers": {"x-portkey-api-key": "${PORTKEY_API_KEY}"},
        "models": ["@openai/gpt-4o"]
    });
    if let Some(metadata) = metadata {
        provider["model_metadata"] = metadata;
    }
    json!({
        "provider": "portkey",
        "model": "@openai/gpt-4o",
        "providers": {"portkey": provider}
    })
}

fn conversation(request: &RecordedRequest) -> Vec<Value> {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] != "system")
        .cloned()
        .collect()
}

fn texts(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .map(|message| {
            format!(
                "{}: {}",
                message["role"].as_str().expect("a role"),
                message["content"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

fn session_id(result: &Value) -> String {
    let id = result["session_id"].as_str().expect("a session id");
    assert_eq!(id.len(), 12, "{result}");
    id.to_owned()
}

#[test]
fn ask_saves_its_turn_and_resuming_sends_it_ahead_of_the_next_prompt() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["one"])),
        Reply::sse(&chat_text_events(&["two"])),
        Reply::sse(&chat_text_events(&["three"])),
        Reply::sse(&chat_text_events(&["four"])),
    ]);
    let home = Home::new(&server.base_url());
    let first = home.ask_json(&["first"], &[]);
    assert_eq!(first["final_output"], "one");
    let id = session_id(&first);
    assert_eq!(home.session_ids(), std::slice::from_ref(&id));
    let directory = home.sessions().join(&id);
    assert_eq!(
        fs::metadata(&directory)
            .expect("session folder")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let metadata = home.metadata(&id);
    assert_eq!(metadata["schema_version"], 4);
    assert_eq!(metadata["provider"]["name"], "portkey");
    assert_eq!(
        metadata["provider"]["binding"]
            .as_str()
            .expect("a binding")
            .len(),
        64
    );
    assert_eq!(metadata["model"], "@openai/gpt-4o");
    assert_eq!(metadata["effort"], "auto");
    assert_eq!(metadata["fast_mode"], false);
    let kinds: Vec<String> = home
        .frames(&id)
        .iter()
        .map(|frame| {
            frame["event"]
                .as_object()
                .and_then(|event| event.keys().next())
                .expect("an event kind")
                .clone()
        })
        .collect();
    assert_eq!(kinds, ["user", "assistant", "turn_completed"]);
    assert!(!directory.join("owner.live").exists());

    assert!(!home.sessions().join(".resume-catalog").exists());
    let second = home.ask_json(&["--resume", "last", "second"], &[]);
    assert_eq!(second["final_output"], "two");
    assert!(home.sessions().join(".resume-catalog").is_file());
    assert_eq!(session_id(&second), id);
    let third = home.ask_json(&["--resume-id", &id, "third"], &[]);
    assert_eq!(session_id(&third), id);
    let fourth = home.ask_json(&["--resume", &id, "fourth"], &[]);
    assert_eq!(session_id(&fourth), id);
    assert_eq!(home.session_ids(), std::slice::from_ref(&id));
    assert_eq!(home.frames(&id).len(), 12);

    let requests = server.requests();
    assert_eq!(texts(&conversation(&requests[0])), ["user: first"]);
    assert_eq!(
        texts(&conversation(&requests[3])),
        [
            "user: first",
            "assistant: one",
            "user: second",
            "assistant: two",
            "user: third",
            "assistant: three",
            "user: fourth",
        ]
    );
}

#[test]
fn ask_saves_the_conversation_language_of_its_prompt() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["один"])),
        Reply::sse(&chat_text_events(&["two"])),
        Reply::sse(&chat_text_events(&["three"])),
    ]);
    let home = Home::new(&server.base_url());
    let first = home.ask_json(&["Проверь этот файл"], &[]);
    let id = session_id(&first);
    assert_eq!(home.metadata(&id)["conversation_language"], "und-Cyrl");
    home.ask_json(&["--resume", &id, "42?"], &[]);
    assert_eq!(home.metadata(&id)["conversation_language"], "und-Cyrl");
    home.ask_json(&["--resume", &id, "ランディングページを開いて"], &[]);
    assert_eq!(home.metadata(&id)["conversation_language"], "ja");
}

#[test]
fn resumed_tool_steps_are_sent_exactly_as_the_model_first_saw_them() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "read_file",
            r#"{"path":"notes.txt"}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_2",
            "read_file",
            r#"{"path":"missing.txt"}"#,
        )),
        Reply::sse(&chat_text_events(&["Read it."])),
        Reply::sse(&chat_text_events(&["Again."])),
    ]);
    let home = Home::new(&server.base_url());
    fs::write(
        home.root.join("workspace/notes.txt"),
        "alpha\n".repeat(2000),
    )
    .expect("write the notes");
    let first = home.ask_json(&["read the notes"], &[]);
    assert_eq!(first["final_output"], "Read it.");
    let id = session_id(&first);
    let results = home.sessions().join(&id).join("tool-results");
    assert_eq!(
        fs::metadata(&results)
            .expect("tool results folder")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(fs::read_dir(&results).expect("tool results").count(), 2);
    home.ask_json(&["--resume", "last", "and again"], &[]);
    let requests = server.requests();
    let live = conversation(&requests[2]);
    let resumed = conversation(&requests[3]);
    assert_eq!(live.len(), 5);
    assert_eq!(resumed[..5], live[..]);
    assert_eq!(
        texts(&resumed[5..]),
        ["assistant: Read it.", "user: and again"]
    );
}

#[test]
fn an_unauthorized_first_request_leaves_no_session_behind() {
    let server = FakeServer::start([Reply::status(401, r#"{"error":{"message":"bad key"}}"#)]);
    let home = Home::new(&server.base_url());
    let result = home.ask_json(&["hello"], &[]);
    assert_eq!(result["session_id"], "");
    assert_eq!(result["auth_failure"]["http_status"], 401);
    assert!(home.session_ids().is_empty());
}

#[test]
fn other_failures_keep_the_session_they_started() {
    let server = FakeServer::start([Reply::status(400, r#"{"error":{"message":"bad"}}"#)]);
    let home = Home::new(&server.base_url());
    let result = home.ask_json(&["hello"], &[]);
    let id = session_id(&result);
    assert_eq!(home.session_ids(), std::slice::from_ref(&id));
    assert!(home.frames(&id).is_empty());
}

#[test]
fn a_saved_run_stopped_after_a_retry_can_be_continued_from_its_checkpoint() {
    let server = FakeServer::start([
        Reply::status(429, r#"{"error":{"message":"slow down"}}"#),
        Reply::status(400, r#"{"error":{"message":"bad"}}"#),
        Reply::sse(&chat_text_events(&["fresh"])),
    ]);
    let home = Home::new(&server.base_url());
    let result = home.ask_json(&["lost"], &[]);
    let id = session_id(&result);
    assert_eq!(result["exit_code"], 1);
    assert_eq!(
        result["recovery"],
        json!({"state":"active","kind":"auto_retry","cause":"rate_limited","action":"retrying_request","attempt":2,"attempt_limit":10,"delay_seconds":0,"durable":true,"message":"⚠ Rate limited · HTTP 429 · slow down · retrying request"})
    );
    assert_eq!(home.session_ids(), std::slice::from_ref(&id));
    assert!(home.frames(&id).is_empty());
    let saved: Value = serde_json::from_slice(
        &fs::read(home.sessions().join(&id).join("recovery.json")).expect("a recovery checkpoint"),
    )
    .expect("checkpoint JSON");
    assert_eq!(saved["conversation_seq"], 0);
    let checkpoint = &saved["checkpoint"];
    assert_eq!(checkpoint["user"]["text"], "lost");
    assert_eq!(checkpoint["cause"], "rate_limited");
    assert_eq!(checkpoint["action"], "retrying_request");
    assert_eq!(checkpoint["consumed_provider_attempts"], 1);
    assert_eq!(checkpoint["authority"]["credential_source"], "configured");
    assert_eq!(
        checkpoint["authority"]["credential_identity"],
        CONFIGURED_IDENTITY
    );
    assert_eq!(
        checkpoint["authority"]["provider"],
        home.metadata(&id)["provider"]
    );
    let resumed = home.ask_json(&["--resume-id", &id, "--continue-recovery"], &[]);
    assert_eq!(session_id(&resumed), id);
    assert_eq!(resumed["final_output"], "fresh");
    assert!(!home.sessions().join(&id).join("recovery.json").exists());
    assert_eq!(
        kinds(&home.frames(&id)),
        ["user", "assistant", "turn_completed"]
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(texts(&conversation(&requests[1])), ["user: lost"]);
    assert_eq!(texts(&conversation(&requests[2])), ["user: lost"]);
    let output = home.ask(&["ask", "--resume-id", &id, "--continue-recovery"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "oh-fx: NoPendingRecovery\n"
    );
    let output = home.ask(
        &["ask", "--json", "--resume-id", &id, "--continue-recovery"],
        &[],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let result: Value = serde_json::from_slice(&output.stdout).expect("a JSON result");
    assert_eq!(result["error"], "NoPendingRecovery");
}

#[test]
fn resuming_fails_before_any_request_when_the_session_cannot_be_opened() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url());
    for (args, code) in [
        (&["--resume", "last", "hi"][..], "NoSavedSessions"),
        (&["--resume-id", "missing", "hi"], "SessionNotFound"),
        (&["--resume", "../escape", "hi"], "InvalidSessionId"),
    ] {
        let output = home.ask(&[&["ask"][..], args].concat(), &[]);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("oh-fx: {code}\n"),
            "{args:?}"
        );
        let result = home.ask_json(args, &[]);
        assert_eq!(result["error"], code, "{args:?}");
        assert_eq!(result["session_id"], "", "{args:?}");
    }
    assert!(server.requests().is_empty());
}

#[test]
fn a_resumed_session_keeps_its_model_until_the_environment_or_a_flag_overrides_it() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["one"])),
        Reply::sse(&chat_text_events(&["two"])),
        Reply::sse(&chat_text_events(&["three"])),
        Reply::sse(&chat_text_events(&["four"])),
    ]);
    let home = Home::new(&server.base_url());
    let first = home.ask_json(
        &["--model", "@flag/model", "--effort", "high", "first"],
        &[],
    );
    let id = session_id(&first);
    let metadata = home.metadata(&id);
    assert_eq!(metadata["model"], "@openai/gpt-4o");
    assert_eq!(metadata["effort"], "auto");
    let mut changed = settings(&server.base_url(), None);
    changed["model"] = json!("@settings/model");
    home.write_settings(&changed);
    home.ask_json(&["--resume", "last", "second"], &[]);
    home.ask_json(
        &["--resume", "last", "third"],
        &[("OH_FX_MODEL", "@environment/model")],
    );
    home.ask_json(
        &["--resume", "last", "--model", "@flag/model", "fourth"],
        &[],
    );
    let models: Vec<String> = server
        .requests()
        .iter()
        .map(|request| {
            request.json()["model"]
                .as_str()
                .expect("a model")
                .to_owned()
        })
        .collect();
    assert_eq!(
        models,
        [
            "@flag/model",
            "@openai/gpt-4o",
            "@environment/model",
            "@flag/model"
        ]
    );
    assert_eq!(home.metadata(&id)["model"], "@openai/gpt-4o");
}

#[test]
fn a_changed_provider_definition_refuses_to_resume_its_sessions() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
    let home = Home::new(&server.base_url());
    home.ask_json(&["first"], &[]);
    let mut changed = settings(&server.base_url(), None);
    changed["providers"]["portkey"]["base_url"] = json!("http://127.0.0.1:9/v1");
    home.write_settings(&changed);
    let result = home.ask_json(&["--resume", "last", "second"], &[]);
    assert_eq!(result["error"], "ConfiguredProviderChanged");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_compaction_during_ask_saves_a_checkpoint_that_resumed_sessions_start_from() {
    let big = format!("FIRST_SENTINEL {}", "word ".repeat(30_000));
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&[&big])),
        Reply::sse(&chat_text_events(&["two"])),
        Reply::sse(&chat_text_events(&["three"])),
    ]);
    let metadata = json!({"@openai/gpt-4o": {"context_window": 40000, "max_output_tokens": 1000}});
    let home = Home::with_settings(&settings(&server.base_url(), Some(metadata)));
    let first = home.ask_json(&["first"], &[]);
    let id = session_id(&first);
    let second = home.ask_json(&["--resume", "last", "second"], &[]);
    assert_eq!(second["final_output"], "two", "{second}");
    let frames = home.frames(&id);
    let checkpoint = frames
        .iter()
        .find_map(|frame| frame["event"].get("context_checkpoint"))
        .expect("a saved checkpoint");
    assert_eq!(checkpoint["covers_through_seq"], 3);
    assert!(
        checkpoint["summary"]
            .as_str()
            .expect("checkpoint summary")
            .starts_with("fx-compactor-v1\n{")
    );
    home.ask_json(&["--resume", "last", "third"], &[]);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let live = conversation(&requests[1]);
    let resumed = conversation(&requests[2]);
    let rendered = live[0]["content"].as_str().expect("checkpoint text");
    assert!(
        rendered.starts_with("<compacted_conversation>\n"),
        "{rendered}"
    );
    assert!(rendered.contains("FIRST_SENTINEL"));
    assert_eq!(live.len(), 2);
    assert_eq!(resumed[..2], live[..]);
    assert_eq!(texts(&resumed[2..]), ["assistant: two", "user: third"]);
}

#[test]
fn a_turn_whose_tool_result_cannot_be_saved_after_a_checkpoint_resumes_closed() {
    let big = format!("FIRST_SENTINEL {}", "word ".repeat(30_000));
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&[&big])),
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "read_file",
            r#"{"path":"small.txt"}"#,
        )),
        Reply::sse(&chat_text_events(&["Read it."])),
        Reply::sse(&chat_text_events(&["three"])),
    ]);
    let metadata = json!({"@openai/gpt-4o": {"context_window": 40000, "max_output_tokens": 1000}});
    let home = Home::with_settings(&settings(&server.base_url(), Some(metadata)));
    fs::write(home.root.join("workspace/small.txt"), "alpha\n").expect("write small.txt");
    let id = session_id(&home.ask_json(&["first"], &[]));
    let blocker = home.sessions().join(&id).join("tool-results");
    fs::write(&blocker, "blocked").expect("block the tool results");
    let second = home.ask_json(&["--resume", "last", "second"], &[]);
    assert_eq!(second["error"], "SessionPathUnsafe", "{second}");
    assert_eq!(
        kinds(&home.frames(&id)),
        [
            "user",
            "assistant",
            "turn_completed",
            "user",
            "context_checkpoint"
        ]
    );
    fs::remove_file(&blocker).expect("unblock the tool results");
    let third = home.ask_json(&["--resume", "last", "third"], &[]);
    assert_eq!(third["final_output"], "three", "{third}");
    assert_eq!(
        kinds(&home.frames(&id))[5..],
        ["interrupted", "user", "assistant", "turn_completed"]
    );
    let resumed = texts(&conversation(&server.requests()[3]));
    assert_eq!(resumed.last().map(String::as_str), Some("user: third"));
    assert!(
        resumed.iter().all(|text| text != "assistant: three"),
        "{resumed:?}"
    );
}

#[test]
fn no_save_runs_neither_create_nor_resume_sessions() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
    let home = Home::new(&server.base_url());
    let result = home.ask_json(&["--no-save", "hello"], &[]);
    assert_eq!(result["session_id"], "");
    assert!(!home.root.join("data").exists());
    let output = home.ask(&["ask", "--no-save", "--resume", "last", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
}

fn chunk(delta: &Value, finish: &Value) -> String {
    json!({
        "id": "chatcmpl-sessions",
        "object": "chat.completion.chunk",
        "model": "sessions-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
    .to_string()
}

fn spoken_read_events(content: &str, call_id: &str, path: &str) -> Vec<String> {
    let call = json!([{
        "index": 0,
        "id": call_id,
        "type": "function",
        "function": {"name": "read_file", "arguments": json!({"path": path}).to_string()},
    }]);
    vec![
        chunk(
            &json!({"role": "assistant", "content": content}),
            &Value::Null,
        ),
        chunk(&json!({"tool_calls": call}), &Value::Null),
        chunk(&json!({}), &json!("tool_calls")),
        "[DONE]".to_owned(),
    ]
}

fn unmetered_text_events(text: &str) -> Vec<String> {
    vec![
        chunk(&json!({"role": "assistant", "content": text}), &Value::Null),
        chunk(&json!({}), &json!("stop")),
        "[DONE]".to_owned(),
    ]
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

fn small_window_home(server: &FakeServer) -> Home {
    let metadata = json!({"@openai/gpt-4o": {"context_window": 45000, "max_output_tokens": 64}});
    let home = Home::with_settings(&settings(&server.base_url(), Some(metadata)));
    fs::write(home.root.join("workspace/small.txt"), "alpha\n").expect("write small.txt");
    let notes = (0..2000).fold(String::new(), |mut notes, line| {
        let _ = writeln!(notes, "w{line:07}");
        notes
    });
    fs::write(home.root.join("workspace/notes.txt"), notes).expect("write notes.txt");
    home
}

const NOTES_REPLY: &str = "Turn in progress\nIn between: Read the notes.\nT1: read small.txt";

#[test]
fn a_compaction_inside_a_turn_resumes_with_the_requests_the_model_saw() {
    let big = format!("STEP_SENTINEL {}", "h".repeat(120_000));
    let server = FakeServer::start([
        Reply::sse(&spoken_read_events(&big, "call_1", "small.txt")),
        Reply::sse(&spoken_read_events("second", "call_2", "notes.txt")),
        Reply::sse(&unmetered_text_events(NOTES_REPLY)),
        Reply::sse(&spoken_read_events("third", "call_3", "small.txt")),
        Reply::sse(&unmetered_text_events("done")),
        Reply::sse(&unmetered_text_events("again")),
    ]);
    let home = small_window_home(&server);
    let first = home.ask_json(&["read the notes"], &[]);
    let id = session_id(&first);
    assert!(
        kinds(&home.frames(&id)).contains(&"context_checkpoint".to_owned()),
        "{first}"
    );
    home.ask_json(&["--resume", "last", "next"], &[]);
    let requests = server.requests();
    let live = conversation(&requests[requests.len() - 2]);
    let resumed = conversation(&requests[requests.len() - 1]);
    assert!(
        live[0]["content"]
            .as_str()
            .expect("checkpoint text")
            .starts_with("<compacted_conversation>\n")
    );
    assert_eq!(resumed[..live.len()], live[..]);
    assert_eq!(
        texts(&resumed[live.len()..]),
        [
            format!(
                "assistant: {}",
                first["final_output"].as_str().expect("a reply")
            ),
            "user: next".to_owned()
        ]
    );
}

#[test]
fn a_crash_after_a_compaction_inside_a_turn_resumes_with_the_turn_closed() {
    let big = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let server = FakeServer::start([
        Reply::sse(&spoken_read_events(&big, "call_1", "small.txt")),
        Reply::sse(&unmetered_text_events(NOTES_REPLY)),
        Reply::held_sse(&[chunk(
            &json!({"role": "assistant", "content": "partial"}),
            &Value::Null,
        )]),
        Reply::sse(&unmetered_text_events("again")),
    ]);
    let home = small_window_home(&server);
    let mut child = home
        .command(&["ask", "--json", "read the notes"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start oh-fx");
    let deadline = Instant::now() + Duration::from_secs(30);
    while server.requests().len() < 3 {
        assert!(Instant::now() < deadline, "{:?}", server.requests().len());
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("kill oh-fx");
    child.wait().expect("reap oh-fx");
    let id = home.session_ids().pop().expect("a session");
    assert_eq!(
        kinds(&home.frames(&id)),
        [
            "user",
            "assistant",
            "tool_call",
            "tool_result",
            "context_checkpoint"
        ]
    );
    let resumed = home.ask_json(&["--resume", "last", "next"], &[]);
    assert_eq!(resumed["final_output"], "again", "{resumed}");
    assert_eq!(
        kinds(&home.frames(&id))[5..],
        ["interrupted", "user", "assistant", "turn_completed"]
    );
    let requests = server.requests();
    let live = conversation(&requests[2]);
    let resumed = conversation(&requests[3]);
    assert_eq!(texts(&live)[1..], ["user: read the notes"]);
    assert_eq!(resumed[..live.len()], live[..]);
    assert_eq!(
        texts(&resumed[live.len()..]),
        [
            "assistant: The previous response ended before completion.",
            "user: <turn_aborted>\nThe previous turn ended before completion. Any tools or commands may have partially executed. Do not continue this request unless the user explicitly asks to continue.\n</turn_aborted>",
            "user: next",
        ]
    );
}

#[test]
fn an_unusable_store_warns_and_runs_without_saving() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
    let home = Home::new(&server.base_url());
    fs::create_dir_all(home.root.join("data")).expect("create the data directory");
    fs::write(home.root.join("data/oh-fx"), "not a directory").expect("block the store");
    let output = home.ask(&["ask", "--json", "hello"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "oh-fx ask: warning: session persistence unavailable; error=SessionPathUnsafe; continuing without saving\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("a JSON result");
    assert_eq!(result["final_output"], "one");
    assert_eq!(result["session_id"], "");
    let output = home.ask(&["ask", "--resume", "last", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "oh-fx: SessionPathUnsafe\n"
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_session_the_open_store_cannot_start_fails_the_run_before_any_request() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url());
    let model = "m".repeat(1025);
    let unsavable = [("OH_FX_MODEL", model.as_str())];
    let output = home.ask(&["ask", "hello"], &unsavable);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "oh-fx: InvalidDurableField\n"
    );
    let result = home.ask_json(&["hello"], &unsavable);
    assert_eq!(result["error"], "InvalidDurableField");
    assert_eq!(result["session_id"], "");
    assert!(home.session_ids().is_empty());
    assert!(server.requests().is_empty());
}

#[test]
fn an_interrupted_turn_is_saved_and_resumes_with_its_partial_reply_closed() {
    let server = FakeServer::start([
        Reply::sse(&spoken_read_events("", "call_1", "small.txt")),
        Reply::held_sse(&[chunk(
            &json!({"role": "assistant", "content": "half"}),
            &Value::Null,
        )]),
        Reply::sse(&unmetered_text_events("again")),
    ]);
    let home = Home::new(&server.base_url());
    fs::write(home.root.join("workspace/small.txt"), "alpha\n").expect("write small.txt");
    let mut child = home
        .command(&["ask", "read it"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start oh-fx");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut seen = Vec::new();
    let mut byte = [0_u8; 1];
    while !seen.ends_with(b"half") {
        stdout
            .read_exact(&mut byte)
            .expect("read the partial reply");
        seen.push(byte[0]);
    }
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(killed.success());
    assert_eq!(child.wait().expect("reap oh-fx").signal(), Some(2));
    let id = home.session_ids().pop().expect("a session");
    let frames = home.frames(&id);
    assert_eq!(
        kinds(&frames),
        ["user", "tool_call", "tool_result", "interrupted"]
    );
    let interrupted = &frames[3]["event"]["interrupted"];
    assert_eq!(interrupted["reason"], "cancelled");
    assert_eq!(interrupted["partial_text"], "half");
    let resumed = home.ask_json(&["--resume", "last", "next"], &[]);
    assert_eq!(resumed["final_output"], "again", "{resumed}");
    let requests = server.requests();
    let live = conversation(&requests[1]);
    let resumed = conversation(&requests[2]);
    assert_eq!(resumed[..live.len()], live[..]);
    assert_eq!(
        texts(&resumed[live.len()..]),
        [
            "assistant: half\n\nThe previous response ended before completion.",
            "user: <turn_aborted>\nThe previous turn ended before completion. Any tools or commands may have partially executed. Do not continue this request unless the user explicitly asks to continue.\n</turn_aborted>",
            "user: next",
        ]
    );
}

const CONFIGURED_IDENTITY: &str =
    "40122b758656199048961e6e8369383c25ebcdeddced75b64ad736e527014da8";
const CONTINUE_AFTER_TOOL: &str =
    "Continue from the confirmed tool result above without repeating the tool.";

fn save_checkpoint(home: &Home, id: &str, credential: &str) {
    let provider = home.metadata(id)["provider"].clone();
    let checkpoint = json!({
        "version": 2,
        "turn_id": 7,
        "user": {"text": "fix the build", "images": []},
        "assistant_source": "",
        "execution": {
            "schema_version": 10,
            "tool_steps": [{
                "assistant": null,
                "provider_replay": null,
                "tool_calls": [{"id": "call_1", "name": "read", "arguments_json": "{\"path\":\"a.rs\"}", "provider_result": null}],
                "tool_results": [{
                    "tool_call_id": "call_1", "tool_name": "read", "status": "success",
                    "output": "fn main() {}", "output_handle": null, "preview": null,
                    "output_bytes": 12, "stored_output_bytes": 12, "truncated": false,
                    "provider_native": false, "review_feedback": false, "created_at_ms": 5,
                    "permission_feedback": [], "committed_file_presentation": null,
                    "command_output_replay": null, "command_process_presentation": null,
                    "terminal_action_presentation": null
                }]
            }],
            "files": [],
            "steering": [],
            "turn_summary": null
        },
        "cause": "response_interrupted",
        "action": "continuing_after_tool",
        "tool_state": "confirmed",
        "authority": {
            "provider": provider,
            "model": "@openai/gpt-4o",
            "credential_source": "configured",
            "credential_identity": credential
        },
        "requested_fast_mode": false,
        "fast_mode": false,
        "max_provider_attempts": 10,
        "consumed_provider_attempts": 1,
        "outstanding_reservation": false
    });
    let seq = home.frames(id).len();
    fs::write(
        home.sessions().join(id).join("recovery.json"),
        format!("{{\"conversation_seq\":{seq},\"checkpoint\":{checkpoint}}}\n"),
    )
    .expect("write recovery.json");
}

#[test]
fn continue_recovery_resumes_a_paused_turn_from_its_saved_tool_steps() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["one"])),
        Reply::sse(&chat_text_events(&["continued"])),
    ]);
    let home = Home::new(&server.base_url());
    let id = session_id(&home.ask_json(&["first"], &[]));
    save_checkpoint(&home, &id, CONFIGURED_IDENTITY);
    let result = home.ask_json(&["--resume-id", &id, "--continue-recovery"], &[]);
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["final_output"], "continued");
    assert_eq!(session_id(&result), id);
    let requests = server.requests();
    let sent = conversation(&requests[1]);
    assert_eq!(
        texts(&sent),
        [
            "user: first",
            "assistant: one",
            "user: fix the build",
            "assistant: ",
            "tool: fn main() {}",
            &format!("user: {CONTINUE_AFTER_TOOL}"),
        ]
    );
    assert_eq!(sent[3]["tool_calls"][0]["id"], "call_1");
    assert!(!home.sessions().join(&id).join("recovery.json").exists());
    assert_eq!(
        kinds(&home.frames(&id)),
        [
            "user",
            "assistant",
            "turn_completed",
            "user",
            "tool_call",
            "tool_result",
            "assistant",
            "turn_completed"
        ]
    );
    let again = home.ask_json(&["--resume-id", &id, "--continue-recovery"], &[]);
    assert_eq!(again["error"], "NoPendingRecovery");
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn continue_recovery_refuses_a_possibly_sent_request_under_another_credential() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
    let home = Home::new(&server.base_url());
    let id = session_id(&home.ask_json(&["first"], &[]));
    save_checkpoint(&home, &id, &"ab".repeat(32));
    let result = home.ask_json(&["--resume-id", &id, "--continue-recovery"], &[]);
    assert_eq!(
        result["error"], "RecoveryCredentialAuthorityChanged",
        "{result}"
    );
    assert_eq!(server.requests().len(), 1);
    assert!(home.sessions().join(&id).join("recovery.json").exists());
}

#[test]
fn a_new_prompt_leaves_a_paused_turn_out_and_clears_it_once_saved() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["one"])),
        Reply::sse(&chat_text_events(&["fresh"])),
    ]);
    let home = Home::new(&server.base_url());
    let id = session_id(&home.ask_json(&["first"], &[]));
    save_checkpoint(&home, &id, CONFIGURED_IDENTITY);
    let result = home.ask_json(&["--resume-id", &id, "next"], &[]);
    assert_eq!(result["final_output"], "fresh");
    assert!(!home.sessions().join(&id).join("recovery.json").exists());
    assert_eq!(
        texts(&conversation(&server.requests()[1])),
        ["user: first", "assistant: one", "user: next"]
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
