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

use ofx_contract::{INTERRUPTED_BEFORE_COMPLETION, INTERRUPTED_TURN_CONTEXT};
use ofx_testkit::{FakeServer, RecordedRequest, Reply, chat_text_events, chat_tool_call_events};
use serde_json::{Value, json};

const KEY: [(&str, &str); 1] = [("PORTKEY_API_KEY", "pk-test-0123456789")];
const FILE_EVIDENCE: &str = "Session file evidence from previous tool execution. Re-read stale paths before relying on exact contents:";

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
        [
            format!(
                "user: {FILE_EVIDENCE}\n- action=read status=success path=notes.txt tool=read_file\n- action=read status=failure path=missing.txt tool=read_file"
            ),
            "assistant: Read it.".to_owned(),
            "user: and again".to_owned()
        ]
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
                "user: {FILE_EVIDENCE}\n- action=read status=success path=small.txt model_view=full tool=read_file\n- action=read status=success path=small.txt model_view=full tool=read_file"
            ),
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
            format!("assistant: {INTERRUPTED_BEFORE_COMPLETION}"),
            format!("user: {INTERRUPTED_TURN_CONTEXT}"),
            "user: next".to_owned(),
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
            format!(
                "user: {FILE_EVIDENCE}\n- action=read status=success path=small.txt model_view=full tool=read_file"
            ),
            format!("assistant: half\n\n{INTERRUPTED_BEFORE_COMPLETION}"),
            format!("user: {INTERRUPTED_TURN_CONTEXT}"),
            "user: next".to_owned(),
        ]
    );
}

const CONFIGURED_IDENTITY: &str =
    "40122b758656199048961e6e8369383c25ebcdeddced75b64ad736e527014da8";
const CONTINUE_AFTER_TOOL: &str =
    "Continue from the confirmed tool result above without repeating the tool.";
const CONTINUE_RESPONSE: &str = "The previous response was interrupted. Restart that response from the beginning using the completed tool results above. Do not repeat completed tool actions.";

fn save_checkpoint(home: &Home, id: &str, credential: &str) {
    save_checkpoint_with(home, id, credential, |_| {});
}

fn save_checkpoint_with(home: &Home, id: &str, credential: &str, change: impl Fn(&mut Value)) {
    let provider = home.metadata(id)["provider"].clone();
    let mut checkpoint = json!({
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
    change(&mut checkpoint);
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
fn a_continued_turn_saves_its_restored_results_with_their_raw_size_and_process() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["one"])),
        Reply::sse(&chat_text_events(&["continued"])),
    ]);
    let home = Home::new(&server.base_url());
    let id = session_id(&home.ask_json(&["first"], &[]));
    save_checkpoint(&home, &id, CONFIGURED_IDENTITY);
    let path = home.sessions().join(&id).join("recovery.json");
    let saved = fs::read_to_string(&path).expect("read recovery.json");
    let failed = saved
        .replace(r#""output_bytes":12"#, r#""output_bytes":40"#)
        .replace(
            r#""command_process_presentation":null"#,
            r#""command_process_presentation":{"kind":"exit_code","value":3}"#,
        );
    assert_ne!(failed, saved);
    fs::write(&path, failed).expect("write recovery.json");
    let result = home.ask_json(&["--resume-id", &id, "--continue-recovery"], &[]);
    assert_eq!(result["final_output"], "continued", "{result}");
    let frames = home.frames(&id);
    let restored = frames
        .iter()
        .find_map(|frame| frame["event"].get("tool_result"))
        .expect("the restored result");
    assert_eq!(restored["output_bytes"], 40);
    assert_eq!(
        restored["command_process_presentation"],
        json!({"exit_code": 3})
    );
}

#[test]
fn continue_recovery_shows_the_saved_partial_reply_and_restarts_it() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["one"])),
        Reply::sse(&chat_text_events(&["continued"])),
    ]);
    let home = Home::new(&server.base_url());
    let id = session_id(&home.ask_json(&["first"], &[]));
    save_checkpoint_with(&home, &id, CONFIGURED_IDENTITY, |checkpoint| {
        checkpoint["assistant_source"] = json!("Looking at");
        checkpoint["cause"] = json!("network_interrupted");
        checkpoint["action"] = json!("continuing_response");
        checkpoint["tool_state"] = json!("none");
    });
    let output = home.ask(&["ask", "--resume-id", &id, "--continue-recovery"], &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Looking at\n\n[Response interrupted. Restarting.]\n\ncontinued"
    );
    let sent = conversation(&server.requests()[1]);
    assert_eq!(
        texts(&sent).last().map(String::as_str),
        Some(format!("user: {CONTINUE_RESPONSE}").as_str())
    );
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

fn sent_shell_arguments(request: &RecordedRequest) -> Vec<String> {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| {
            message["tool_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .map(|call| {
            call["function"]["arguments"]
                .as_str()
                .expect("arguments")
                .to_owned()
        })
        .collect()
}

#[test]
fn shell_calls_are_saved_as_upstream_saves_them_and_sent_back_in_the_request_form() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "shell",
            r#"{ "request" : { "action" : "run" } }"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_2",
            "shell",
            r#"{"action":"run","command":"","timeout_ms":5E3}"#,
        )),
        Reply::sse(&chat_text_events(&["Done."])),
        Reply::sse(&chat_text_events(&["Again."])),
    ]);
    let home = Home::new(&server.base_url());
    let first = home.ask_json(&["run it"], &[]);
    let id = session_id(&first);
    let saved: Vec<String> = home
        .frames(&id)
        .iter()
        .filter_map(|frame| frame["event"]["tool_call"]["arguments_json"].as_str())
        .map(str::to_owned)
        .collect();
    assert_eq!(
        saved,
        [
            r#"{"action":"run"}"#,
            r#"{"action":"run","command":"","timeout_ms":5E3}"#
        ]
    );
    let resumed = home.ask_json(&["--resume-id", &id, "again"], &[]);
    assert_eq!(resumed["final_output"], "Again.");
    let requests = server.requests();
    let sent = [
        r#"{"request":{"action":"run"}}"#,
        r#"{"request":{"action":"run","command":"","timeout_ms":5000}}"#,
    ];
    assert_eq!(sent_shell_arguments(&requests[2]), sent);
    assert_eq!(sent_shell_arguments(&requests[3]), sent);
}

#[test]
fn a_saved_turn_records_which_files_the_model_read_whole_and_which_went_stale() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "read_file",
            r#"{"path":"notes.txt"}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_2",
            "read_file",
            r#"{"path":"long.txt","line_count":1}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_3",
            "write_file",
            r#"{"path":"notes.txt","content":"beta\n"}"#,
        )),
        Reply::sse(&chat_text_events(&["Rewrote it."])),
    ]);
    let home = Home::new(&server.base_url());
    fs::write(home.root.join("workspace/notes.txt"), "alpha\n").expect("write the notes");
    fs::write(home.root.join("workspace/long.txt"), "one\ntwo\n").expect("write the long file");
    let result = home.ask_json(&["--full-access", "rewrite the notes"], &[]);
    assert_eq!(result["final_output"], "Rewrote it.");
    let frames = home.frames(&session_id(&result));
    let completed = frames
        .iter()
        .find_map(|frame| frame["event"].get("turn_completed"))
        .expect("a completed turn");
    let files: Vec<(String, String, String, bool, bool)> = completed["files"]
        .as_array()
        .expect("file evidence")
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap().to_owned(),
                file["action"].as_str().unwrap().to_owned(),
                file["status"].as_str().unwrap().to_owned(),
                file["model_view_covers_full_file"].as_bool().unwrap(),
                file["stale"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        files,
        [
            (
                "notes.txt".to_owned(),
                "read".to_owned(),
                "success".to_owned(),
                true,
                true
            ),
            (
                "long.txt".to_owned(),
                "read".to_owned(),
                "success".to_owned(),
                false,
                false
            ),
            (
                "notes.txt".to_owned(),
                "write".to_owned(),
                "success".to_owned(),
                false,
                false
            ),
        ]
    );
}

#[test]
fn a_failed_command_saves_its_process_presentation_as_upstream_frames_it() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "shell",
            r#"{"request":{"action":"run","command":"exit 3"}}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_2",
            "shell",
            r#"{"request":{"action":"run","command":"true"}}"#,
        )),
        Reply::sse(&chat_text_events(&["Done."])),
    ]);
    let home = Home::new(&server.base_url());
    let result = home.ask_json(&["--yolo", "run it"], &[]);
    assert_eq!(result["final_output"], "Done.", "{result}");
    let saved: Vec<(String, Value)> = home
        .frames(&session_id(&result))
        .iter()
        .filter_map(|frame| frame["event"].get("tool_result"))
        .map(|result| {
            (
                result["status"].as_str().expect("a status").to_owned(),
                result["command_process_presentation"].clone(),
            )
        })
        .collect();
    assert_eq!(
        saved,
        [
            ("failure".to_owned(), json!({"exit_code": 3})),
            ("success".to_owned(), Value::Null),
        ]
    );
}

const FX_ID: &str = "fx0123456789";

const GATEWAY: &str = "\"gateway\"";

fn save_in_fx(home: &Home, provider: &str) -> PathBuf {
    write_fx_session(home, FX_ID, "/elsewhere/fx-work", 2, provider)
}

fn write_fx_session(
    home: &Home,
    id: &str,
    workspace: &str,
    updated_at_ms: i64,
    provider: &str,
) -> PathBuf {
    let fx = home.root.join(".fx");
    let session = fx.join("sessions").join(id);
    fs::create_dir_all(&session).expect("create an fx session");
    for directory in [fx.clone(), fx.join("sessions"), session.clone()] {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("make an fx folder private");
    }
    let manifest = format!(
        "{{\"schema_version\":4,\"id\":\"{id}\",\"origin_workspace_root\":\"{workspace}\",\"workspace_root\":\"{workspace}\",\"created_at_ms\":1,\"updated_at_ms\":{updated_at_ms},\"conversation_language\":\"en\",\"provider\":{provider},\"model\":\"openai/gpt-5\",\"effort\":\"high\",\"fast_mode\":false,\"title\":\"Started in fx\",\"subagent_child\":false}}"
    );
    let mut events = String::new();
    for (seq, event) in (1_u64..).zip([
        "{\"user\":{\"text\":\"asked in fx\",\"images\":[],\"work_id\":null}}",
        "{\"assistant\":{\"text\":\"answered in fx\",\"provider_replay\":null,\"standalone_response\":false}}",
        "{\"turn_completed\":{\"files\":[],\"turn_summary\":null}}",
    ]) {
        let _ = writeln!(
            events,
            "{{\"schema_version\":3,\"seq\":{seq},\"timestamp_ms\":2,\"event\":{event}}}"
        );
    }
    for (name, bytes) in [
        ("session.json", manifest),
        ("events.jsonl", events),
        ("session.lock", String::new()),
    ] {
        fs::write(session.join(name), bytes).expect("write an fx session file");
        fs::set_permissions(session.join(name), fs::Permissions::from_mode(0o600))
            .expect("make an fx file private");
    }
    session
}

fn fx_files(session: &std::path::Path) -> Vec<Vec<u8>> {
    ["session.json", "events.jsonl", "session.lock"]
        .iter()
        .map(|name| fs::read(session.join(name)).expect("read an fx file"))
        .collect()
}

#[test]
fn ask_resumes_an_fx_session_from_a_copy_on_the_current_provider() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["continued"])),
        Reply::sse(&chat_text_events(&["again"])),
    ]);
    let home = Home::new(&server.base_url());
    let source = save_in_fx(&home, GATEWAY);
    let untouched = fx_files(&source);

    let output = home.ask(&["ask", "--json", "--resume", FX_ID, "keep going"], &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        stderr,
        "oh-fx ask: This session was saved with the gateway provider, which oh-fx cannot use yet; it continues with portkey.\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("a JSON result");
    assert_eq!(result["final_output"], "continued");
    assert_eq!(session_id(&result), FX_ID);
    assert_eq!(
        texts(&conversation(&server.requests()[0])),
        [
            "user: asked in fx",
            "assistant: answered in fx",
            "user: keep going"
        ]
    );
    let metadata = home.metadata(FX_ID);
    assert_eq!(metadata["provider"]["name"], "portkey");
    assert_eq!(metadata["model"], "@openai/gpt-4o");
    assert_eq!(metadata["effort"], "high");
    assert_eq!(metadata["title"], "Started in fx");
    assert_eq!(
        metadata["workspace_root"],
        fs::canonicalize(home.root.join("workspace"))
            .expect("canonical workspace")
            .display()
            .to_string()
    );
    assert_eq!(fx_files(&source), untouched);

    let output = home.ask(&["ask", "--json", "--resume", FX_ID, "and again"], &[]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(home.frames(FX_ID).len(), 9);
    assert_eq!(fx_files(&source), untouched);
}

#[test]
fn ask_refuses_an_fx_session_that_fx_has_open() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url());
    let source = save_in_fx(&home, GATEWAY);
    let lock = fs::File::open(source.join("session.lock")).expect("open fx's lock");
    lock.lock().expect("hold fx's lock");

    let output = home.ask(&["ask", "--resume", FX_ID, "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "oh-fx ask: fx has this session open; close it in fx, then resume it here\n"
    );
    assert!(home.session_ids().is_empty());
    assert!(server.requests().is_empty());
}

#[test]
fn ask_continues_an_fx_session_saved_on_a_provider_oh_fx_does_not_define() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["continued"]))]);
    let home = Home::new(&server.base_url());
    let binding = "ab".repeat(32);
    let source = save_in_fx(
        &home,
        &format!("{{\"name\":\"fx-only\",\"binding\":\"{binding}\"}}"),
    );
    let untouched = fx_files(&source);

    let output = home.ask(&["ask", "--json", "--resume", FX_ID, "keep going"], &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        stderr,
        "oh-fx ask: This session was saved with the fx-only provider, which oh-fx cannot use yet; it continues with portkey.\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("a JSON result");
    assert_eq!(result["final_output"], "continued");
    assert_eq!(
        texts(&conversation(&server.requests()[0])),
        [
            "user: asked in fx",
            "assistant: answered in fx",
            "user: keep going"
        ]
    );
    let metadata = home.metadata(FX_ID);
    assert_eq!(metadata["provider"]["name"], "portkey");
    assert_eq!(metadata["model"], "@openai/gpt-4o");
    assert_eq!(metadata["effort"], "high");
    assert_eq!(fx_files(&source), untouched);
}

#[test]
fn ask_resume_last_opens_the_newest_session_of_this_workspace_saved_by_either_agent() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["first"])),
        Reply::sse(&chat_text_events(&["second"])),
        Reply::sse(&chat_text_events(&["third"])),
    ]);
    let home = Home::new(&server.base_url());
    let workspace = fs::canonicalize(home.root.join("workspace"))
        .expect("canonical workspace")
        .display()
        .to_string();
    write_fx_session(&home, FX_ID, &workspace, 2, GATEWAY);
    let own = session_id(&home.ask_json(&["first"], &[]));
    let resumed = home.ask_json(&["--resume", "last", "second"], &[]);
    assert_eq!(session_id(&resumed), own);
    assert_eq!(home.session_ids(), std::slice::from_ref(&own));

    let newest = write_fx_session(
        &home,
        "fx9876543210",
        &workspace,
        4_102_444_800_000,
        GATEWAY,
    );
    write_fx_session(
        &home,
        "fxelsewhere0",
        "/elsewhere/fx-work",
        4_102_444_900_000,
        GATEWAY,
    );
    let untouched = fx_files(&newest);
    let output = home.ask(&["ask", "--json", "--resume", "last", "third"], &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        stderr,
        "oh-fx ask: This session was saved with the gateway provider, which oh-fx cannot use yet; it continues with portkey.\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("a JSON result");
    assert_eq!(session_id(&result), "fx9876543210");
    assert_eq!(
        texts(&conversation(&server.requests()[2])),
        [
            "user: asked in fx",
            "assistant: answered in fx",
            "user: third"
        ]
    );
    assert_eq!(fx_files(&newest), untouched);
    let mut expected = vec![own, "fx9876543210".to_owned()];
    expected.sort();
    assert_eq!(home.session_ids(), expected);
}

const V3_GENERATION: &str = "01010101010101010101010101010101";

fn save_schema_v3_in_fx(home: &Home) -> PathBuf {
    let fx = home.root.join(".fx");
    let session = fx.join("sessions").join(FX_ID);
    fs::create_dir_all(&session).expect("create an fx session");
    for directory in [fx.clone(), fx.join("sessions"), session.clone()] {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("make an fx folder private");
    }
    let started = format!(
        "{{\"id\":\"{FX_ID}\",\"created_at_ms\":1,\"origin_workspace_root\":\"/elsewhere/fx-work\",\"workspace_root\":\"/elsewhere/fx-work\",\"conversation_language\":\"en\",\"preferences\":{{\"model\":\"openai/gpt-5\",\"effort\":\"high\",\"fast_mode\":false}}}}"
    );
    let turn = "{\"conversation_language\":\"en\",\"total_input_tokens\":7,\"total_output_tokens\":3,\"turn\":{\"kind\":\"assistant\",\"user\":{\"text\":\"asked in fx 0.0.7\",\"images\":[]},\"assistant\":\"answered in fx 0.0.7\",\"execution\":{\"schema_version\":3,\"tool_steps\":[],\"files\":[]}}}";
    let mut events = String::new();
    for (seq, (kind, payload)) in (1_u64..).zip([
        ("session_started", started.as_str()),
        ("history_turn_committed", turn),
    ]) {
        let _ = writeln!(
            events,
            "{{\"schema_version\":1,\"log_generation\":\"{V3_GENERATION}\",\"seq\":{seq},\"event_id\":\"{seq:032x}\",\"timestamp_ms\":{},\"kind\":\"{kind}\",\"payload\":{payload}}}",
            seq * 10
        );
    }
    let watermark = format!(
        "{{\"schema_version\":1,\"session_id\":\"{FX_ID}\",\"log_generation\":\"{V3_GENERATION}\",\"through_seq\":2,\"through_event_id\":\"{:032x}\",\"through_event_log_bytes\":{}}}\n",
        2,
        events.len()
    );
    let authority = format!(
        "{{\"schema_version\":1,\"session_id\":\"{FX_ID}\",\"authority_id\":\"{}\",\"storage_format\":\"event_log_v1\",\"source\":\"native_create\"}}\n",
        "03".repeat(16)
    );
    for (name, bytes) in [
        ("authority.json".to_owned(), authority),
        ("events.jsonl".to_owned(), events),
        (format!("commit.{V3_GENERATION}.json"), watermark),
    ] {
        fs::write(session.join(&name), bytes).expect("write an fx session file");
        fs::set_permissions(session.join(&name), fs::Permissions::from_mode(0o600))
            .expect("make an fx file private");
    }
    session
}

#[test]
fn ask_resumes_a_session_fx_saved_before_its_conversation_layout() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["continued"]))]);
    let home = Home::new(&server.base_url());
    let source = save_schema_v3_in_fx(&home);
    let untouched: Vec<Vec<u8>> = ["authority.json", "events.jsonl"]
        .iter()
        .map(|name| fs::read(source.join(name)).expect("read an fx file"))
        .collect();

    let output = home.ask(&["ask", "--json", "--resume", FX_ID, "keep going"], &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        stderr,
        "oh-fx ask: This session was saved with the gateway provider, which oh-fx cannot use yet; it continues with portkey.\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("a JSON result");
    assert_eq!(result["final_output"], "continued");
    assert_eq!(session_id(&result), FX_ID);
    assert_eq!(
        texts(&conversation(&server.requests()[0])),
        [
            "user: asked in fx 0.0.7",
            "assistant: answered in fx 0.0.7",
            "user: keep going"
        ]
    );
    let metadata = home.metadata(FX_ID);
    assert_eq!(metadata["schema_version"], 4);
    assert_eq!(metadata["title"], "asked in fx 0.0.7");
    assert_eq!(metadata["effort"], "high");
    assert_eq!(home.frames(FX_ID).len(), 6);
    let after: Vec<Vec<u8>> = ["authority.json", "events.jsonl"]
        .iter()
        .map(|name| fs::read(source.join(name)).expect("read an fx file"))
        .collect();
    assert_eq!(after, untouched);
    assert!(!source.join("session.json").exists());
}
