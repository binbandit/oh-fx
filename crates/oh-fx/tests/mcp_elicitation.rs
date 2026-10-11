use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use ofx_testkit::{
    FakeServer, PtySession, RecordedRequest, Reply, chat_text_events, chat_tool_call_events,
};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);

const ELICITING_SERVER: &str = r#"#!/bin/sh
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "$line" > "$MCP_STATE/initialize"
      reply "$id" '{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1.0"}}' ;;
    *'"method":"tools/list"'*)
      reply "$id" '{"tools":[{"name":"profile","description":"Ask for a profile.","inputSchema":{"type":"object"}}]}' ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":"ask-1","method":"elicitation/create","params":{"message":"Who are you?","requestedSchema":{"type":"object","properties":{"name":{"type":"string","title":"Name"}},"required":["name"]}}}\n'
      IFS= read -r answer
      printf '%s\n' "$answer" > "$MCP_STATE/answer"
      reply "$id" '{"content":[{"type":"text","text":"answered"}]}' ;;
  esac
done
"#;

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Home {
    fn new(server: &FakeServer) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        let state = root.join("mcp-state");
        for path in [
            root.join("config/oh-fx"),
            root.join("workspace"),
            state.clone(),
        ] {
            fs::create_dir_all(path).expect("create a directory");
        }
        let settings = json!({
            "permission_mode": "ask",
            "provider": "local",
            "model": "local-model",
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": server.base_url(),
                    "auth": {"type": "none"},
                    "models": ["local-model"]
                }
            }
        });
        fs::write(
            root.join("config/oh-fx/settings.json"),
            settings.to_string(),
        )
        .expect("write settings");
        let script = root.join("eliciting.sh");
        fs::write(&script, ELICITING_SERVER).expect("write the server script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        fs::write(
            root.join("config/oh-fx/mcp.json"),
            json!({"mcp": {"fixture": {"command": "/bin/sh", "args": [script]}}}).to_string(),
        )
        .expect("write mcp.json");
        Self {
            _directory: directory,
            root,
            state,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(["ask", "--full-access", "who am I"])
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("MCP_STATE", &self.state);
        command
    }

    fn state(&self, name: &str) -> String {
        fs::read_to_string(self.state.join(name)).expect("the server recorded it")
    }
}

fn provider() -> FakeServer {
    FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "select_1",
            "mcp_select_tool",
            r#"{"name":"mcp_fixture_profile"}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "mcp_fixture_profile",
            "{}",
        )),
        Reply::sse(&chat_text_events(&["done"])),
    ])
}

fn last_tool_result(request: &RecordedRequest) -> Value {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rfind(|message| message["role"] == "tool")
        .map(|message| message["content"].clone())
        .expect("a tool result")
}

fn wait(session: &PtySession, needle: &str) -> String {
    session
        .wait_for(WAIT, |screen| screen.contains(needle))
        .unwrap_or_else(|screen| panic!("expected {needle:?} on screen:\n{screen}"))
}

#[test]
fn a_terminal_ask_answers_an_mcp_form_on_stderr() {
    let server = provider();
    let home = Home::new(&server);
    let mut session = PtySession::spawn(home.command(), 40, 200).expect("spawn oh-fx in a pty");
    let screen = wait(&session, "Enter a value:");
    assert!(
        screen.contains(
            "MCP server fixture requests Name (required)\nReason: Who are you?\nEnter a value:"
        ),
        "{screen}"
    );
    session.send(b"Ada\r");
    let screen = wait(&session, "Choose a number or enter a value:");
    assert!(
        screen.contains("Review the completed form requested by MCP server fixture.\nReason: Who are you?\nCurrent values:\n- Name: \"Ada\"\nChoose Submit, Edit, Decline, or Cancel.\n  1. Submit\n  2. Edit\n  3. Decline\n  4. Cancel\nChoose a number or enter a value:"),
        "{screen}"
    );
    session.send(b"1\r");
    let status = session.wait_exit(WAIT).and_then(|status| status.code());
    assert_eq!(status, Some(0), "{}", session.screen());
    assert!(
        home.state("initialize")
            .contains(r#""capabilities":{"elicitation":{"form":{}}}"#)
    );
    assert_eq!(
        home.state("answer"),
        "{\"jsonrpc\":\"2.0\",\"id\":\"ask-1\",\"result\":{\"action\":\"accept\",\"content\":{\"name\":\"Ada\"}}}\n"
    );
    let requests = server.requests();
    assert_eq!(
        last_tool_result(&requests[2]),
        r#"{"server":"fixture","tool":"profile","result":{"content":[{"type":"text","text":"answered"}]}}"#
    );
}

#[test]
fn an_ask_without_a_terminal_advertises_no_elicitation_and_refuses_the_request() {
    let server = provider();
    let home = Home::new(&server);
    let output = home
        .command()
        .stdin(Stdio::null())
        .output()
        .expect("run oh-fx");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home.state("initialize").contains(r#""capabilities":{}"#));
    assert_eq!(
        home.state("answer"),
        "{\"jsonrpc\":\"2.0\",\"id\":\"ask-1\",\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}\n"
    );
}
