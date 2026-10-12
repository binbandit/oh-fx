use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, RecordedRequest, Reply, chat_text_events, chat_tool_call_events};
use serde_json::{Value, json};

const KEY: [(&str, &str); 1] = [("PORTKEY_API_KEY", "pk-test-0123456789")];
const BASH: &str = "/bin/bash";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new(base_url: &str, permission_mode: Option<&str>) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        fs::create_dir_all(root.join("config/oh-fx")).expect("create the config directory");
        fs::create_dir_all(root.join("workspace")).expect("create the workspace");
        let mut settings = json!({
            "provider": "portkey",
            "model": "@openai/gpt-4o",
            "providers": {"portkey": {
                "protocol": "openai-chat-completions",
                "base_url": base_url,
                "auth": {"type": "none"},
                "headers": {"x-portkey-api-key": "${PORTKEY_API_KEY}"},
                "models": ["@openai/gpt-4o"]
            }}
        });
        if let Some(mode) = permission_mode {
            settings["permission_mode"] = json!(mode);
        }
        fs::write(
            root.join("config/oh-fx/settings.json"),
            settings.to_string(),
        )
        .expect("write settings.json");
        Self {
            _directory: directory,
            root,
        }
    }

    fn ask(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(["ask", "--json"])
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
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }

    fn ask_json(&self, args: &[&str]) -> Value {
        let output = self.ask(args);
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "{args:?}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
}

fn shell_call(call_id: &str, request: &Value) -> Reply {
    Reply::sse(&chat_tool_call_events(
        call_id,
        "shell",
        &json!({ "request": request }).to_string(),
    ))
}

fn terminal_run(command: &str, yield_time_ms: u32) -> Value {
    json!({
        "action": "run",
        "command": command,
        "tty": true,
        "shell": {"kind": "executable", "path": BASH, "clean_start": true},
        "yield_time_ms": yield_time_ms,
    })
}

fn shell_actions(request: &RecordedRequest) -> Vec<Value> {
    request.json()["tools"]
        .as_array()
        .expect("offered tools")
        .iter()
        .find(|tool| tool["function"]["name"] == "shell")
        .expect("the shell tool")["function"]["parameters"]["properties"]["request"]["oneOf"]
        .as_array()
        .expect("the shell actions")
        .clone()
}

fn tool_result(request: &RecordedRequest) -> String {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .expect("a tool result")["content"]
        .as_str()
        .expect("tool result text")
        .to_owned()
}

fn result_json(request: &RecordedRequest) -> Value {
    let content = tool_result(request);
    serde_json::from_str(&content).unwrap_or_else(|_| panic!("{content}"))
}

#[test]
fn a_saved_ask_offers_terminals_and_runs_a_command_in_one() {
    let server = FakeServer::start([
        shell_call("call_1", &terminal_run("printf 'from a tty\\n'", 30_000)),
        Reply::sse(&chat_text_events(&["Done."])),
    ]);
    let home = Home::new(&server.base_url(), None);
    let answered = home.ask_json(&["--yolo", "go"]);
    assert_eq!(answered["final_output"], "Done.", "{answered}");
    let requests = server.requests();
    let actions = shell_actions(&requests[0]);
    assert_eq!(actions.len(), 4);
    assert_eq!(actions[0]["properties"]["tty"]["type"], "boolean");
    assert_eq!(actions[2]["properties"]["chars"]["type"], "string");
    let result = result_json(&requests[1]);
    assert_eq!(result["backend"], "tty", "{result}");
    assert_eq!(result["persistence"], "session", "{result}");
    assert_eq!(result["state"], "completed", "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert!(
        result["output_delta"]
            .as_str()
            .is_some_and(|output| output.contains("from a tty")),
        "{result}"
    );
}

#[test]
fn an_unsaved_ask_offers_only_processes_and_has_no_terminals() {
    let server = FakeServer::start([
        shell_call("call_1", &terminal_run("true", 0)),
        Reply::sse(&chat_text_events(&["Done."])),
    ]);
    let home = Home::new(&server.base_url(), None);
    let answered = home.ask_json(&["--yolo", "--no-save", "go"]);
    assert_eq!(answered["final_output"], "Done.", "{answered}");
    let requests = server.requests();
    let actions = shell_actions(&requests[0]);
    assert_eq!(actions.len(), 3);
    assert!(
        actions
            .iter()
            .all(|action| action["properties"].get("tty").is_none())
    );
    assert_eq!(
        tool_result(&requests[1]),
        r#"{"error":{"tool":"shell","code":"unavailable","retryable":false}}"#
    );
}

#[test]
fn a_terminal_of_an_ask_that_exited_ends_with_it() {
    let server = FakeServer::start([
        shell_call("call_1", &terminal_run("sleep 30", 0)),
        Reply::sse(&chat_text_events(&["Started."])),
    ]);
    let home = Home::new(&server.base_url(), None);
    let first = home.ask_json(&["--yolo", "start it"]);
    assert_eq!(first["final_output"], "Started.", "{first}");
    let session = first["session_id"].as_str().expect("a saved session");
    let started = result_json(&server.requests()[1]);
    assert_eq!(started["state"], "running", "{started}");
    let terminal = started["session_id"].as_str().expect("a terminal");
    server.push([
        shell_call(
            "call_2",
            &json!({"action": "interact", "session_id": terminal}),
        ),
        Reply::sse(&chat_text_events(&["Ended."])),
    ]);
    let resumed = home.ask_json(&["--yolo", "--resume-id", session, "look again"]);
    assert_eq!(resumed["final_output"], "Ended.", "{resumed}");
    assert_eq!(
        tool_result(&server.requests()[3]),
        json!({"error": {
            "tool": "shell",
            "code": "TerminalEnded",
            "session_id": terminal,
            "message": "This terminal ended when the oh-fx process that started it exited. It cannot be resumed. Start a new terminal with shell action run and tty true.",
            "retryable": false,
        }})
        .to_string()
    );
}
