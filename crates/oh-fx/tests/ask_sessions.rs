use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

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

    fn ask<S: AsRef<OsStr>>(&self, args: &[S], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
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
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
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

    let second = home.ask_json(&["--resume", "last", "second"], &[]);
    assert_eq!(second["final_output"], "two");
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
fn no_save_runs_neither_create_nor_resume_sessions() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"]))]);
    let home = Home::new(&server.base_url());
    let result = home.ask_json(&["--no-save", "hello"], &[]);
    assert_eq!(result["session_id"], "");
    assert!(!home.root.join("data").exists());
    let output = home.ask(&["ask", "--no-save", "--resume", "last", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
}
