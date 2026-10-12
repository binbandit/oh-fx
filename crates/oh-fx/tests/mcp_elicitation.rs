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
      if [ -f "$MCP_STATE/url_required" ]; then
        if [ -f "$MCP_STATE/required_sent" ]; then
          reply "$id" '{"content":[{"type":"text","text":"answered"}]}'
        else
          touch "$MCP_STATE/required_sent"
          if [ -f "$MCP_STATE/notify" ]; then
            printf '{"jsonrpc":"2.0","method":"notifications/elicitation/complete","params":{"elicitationId":"connect-1"}}\n'
          fi
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32042,"message":"URL required","data":%s}}\n' "$id" "$(cat "$MCP_STATE/params")"
        fi
        continue
      fi
      printf '{"jsonrpc":"2.0","id":"ask-1","method":"elicitation/create","params":%s}\n' "$(cat "$MCP_STATE/params")"
      if [ -f "$MCP_STATE/twice" ]; then
        printf '{"jsonrpc":"2.0","id":"ask-2","method":"elicitation/create","params":{"message":"Where are you?","requestedSchema":{"type":"object","properties":{"city":{"type":"string","title":"City"}},"required":["city"]}}}\n'
        IFS= read -r first
        IFS= read -r second
        printf '%s\n%s\n' "$first" "$second" > "$MCP_STATE/answer"
      else
        IFS= read -r answer
        printf '%s\n' "$answer" > "$MCP_STATE/answer"
      fi
      reply "$id" '{"content":[{"type":"text","text":"answered"}]}' ;;
  esac
done
"#;

const LAUNCHER: &str = "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$MCP_STATE/opened\"\n";

const CONNECT_URL: &str = r#"{"mode":"url","message":"Connect your account","url":"https://example.test/connect?state=opaque","elicitationId":"connect-1"}"#;

const NAME_FORM: &str = r#"{"message":"Who are you?","requestedSchema":{"type":"object","properties":{"name":{"type":"string","title":"Name"}},"required":["name"]}}"#;

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Home {
    fn new(server: &FakeServer) -> Self {
        Self::with(server, "ask", NAME_FORM)
    }

    fn with(server: &FakeServer, mode: &str, params: &str) -> Self {
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
        fs::write(state.join("params"), params).expect("write the elicitation");
        let settings = json!({
            "permission_mode": mode,
            "yolo_acknowledged": true,
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
        let launchers = root.join("bin");
        fs::create_dir_all(&launchers).expect("create the launcher directory");
        for name in ["open", "xdg-open"] {
            let launcher = launchers.join(name);
            fs::write(&launcher, LAUNCHER).expect("write the browser launcher");
            fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755))
                .expect("make it executable");
        }
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
        let mut command = self.shell_command();
        command.args(["ask", "--full-access", "who am I"]);
        command
    }

    fn shell_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
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
            .contains(r#""capabilities":{"elicitation":{"form":{},"url":{}}}"#)
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
fn a_terminal_ask_asks_overlapping_forms_one_at_a_time() {
    let server = provider();
    let home = Home::new(&server);
    fs::write(home.state.join("twice"), "").expect("ask for two forms");
    let mut session = PtySession::spawn(home.command(), 40, 200).expect("spawn oh-fx in a pty");
    let screen = wait(&session, "Reason: Who are you?\nEnter a value:");
    assert!(!screen.contains("Where are you?"), "{screen}");
    session.send(b"Ada\r");
    let screen = wait(&session, "- Name: \"Ada\"");
    assert!(!screen.contains("Where are you?"), "{screen}");
    session.send(b"1\r");
    wait(&session, "Reason: Where are you?\nEnter a value:");
    session.send(b"Paris\r");
    wait(&session, "- City: \"Paris\"");
    session.send(b"1\r");
    let status = session.wait_exit(WAIT).and_then(|status| status.code());
    assert_eq!(status, Some(0), "{}", session.screen());
    assert_eq!(
        home.state("answer"),
        "{\"jsonrpc\":\"2.0\",\"id\":\"ask-1\",\"result\":{\"action\":\"accept\",\"content\":{\"name\":\"Ada\"}}}\n{\"jsonrpc\":\"2.0\",\"id\":\"ask-2\",\"result\":{\"action\":\"accept\",\"content\":{\"city\":\"Paris\"}}}\n"
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

fn shell(home: &Home) -> PtySession {
    let session = PtySession::spawn(home.shell_command(), 40, 200).expect("spawn oh-fx in a pty");
    wait(&session, "Run /help for commands");
    let ready = format!(
        "  {:<22}{:<22}{}",
        "fixture", "Ready", "stdio \u{b7} Profile"
    );
    session.send(b"/mcp\r");
    session
        .wait_for(WAIT, |screen| {
            screen.contains("esc close") && screen.contains(&ready)
        })
        .unwrap_or_else(|screen| panic!("expected the fixture to be ready:\n{screen}"));
    session.send(b"\x1b");
    session
        .wait_for(WAIT, |screen| !screen.contains("esc close"))
        .unwrap_or_else(|screen| panic!("the MCP menu stayed open:\n{screen}"));
    session.send(b"who am I\r");
    session
}

fn exit(mut session: PtySession) {
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn the_shell_asks_for_an_mcp_form_in_its_question_panel() {
    let server = provider();
    let home = Home::with(&server, "yolo", NAME_FORM);
    let session = shell(&home);
    let screen = wait(&session, "MCP server fixture requests Name (required)");
    assert!(
        screen.contains("  MCP server fixture requests Name (required)\n  Reason: Who are you?\n"),
        "{screen}"
    );
    session.send(b"Ada\r");
    let screen = wait(&session, "Choose Submit, Edit, Decline, or Cancel.");
    for line in [
        "  Review the completed form requested by MCP server fixture.\n",
        "  - Name: \"Ada\"\n",
        "    1) Submit",
        "    4) Cancel",
        "    5) Other",
    ] {
        assert!(screen.contains(line), "{line}\n{screen}");
    }
    session.send(b"1");
    let screen = wait(&session, "done");
    assert!(!screen.contains("Review the completed form"), "{screen}");
    assert_eq!(
        home.state("answer"),
        "{\"jsonrpc\":\"2.0\",\"id\":\"ask-1\",\"result\":{\"action\":\"accept\",\"content\":{\"name\":\"Ada\"}}}\n"
    );
    assert!(
        home.state("initialize")
            .contains(r#""capabilities":{"elicitation":{"form":{},"url":{}}}"#)
    );
    exit(session);
}

#[test]
fn the_shell_submits_a_short_single_choice_form_from_one_panel() {
    let server = provider();
    let home = Home::with(
        &server,
        "yolo",
        r#"{"message":"Pick a color","requestedSchema":{"type":"object","properties":{"color":{"type":"string","title":"Color","oneOf":[{"const":"red","title":"Red"},{"const":"blue","title":"Blue"}]}},"required":["color"]}}"#,
    );
    let session = shell(&home);
    let screen = wait(&session, "Choose an option to submit.");
    for line in [
        "  MCP server fixture requests Color (required)\n  \n  Reason: Pick a color\n  Choose an option to submit.\n",
        "    1) [1] Red",
        "    2) [2] Blue",
        "    3) Decline",
        "    4) Cancel",
    ] {
        assert!(screen.contains(line), "{line}\n{screen}");
    }
    assert!(!screen.contains("Other"), "{screen}");
    session.send(b"2");
    wait(&session, "done");
    assert_eq!(
        home.state("answer"),
        "{\"jsonrpc\":\"2.0\",\"id\":\"ask-1\",\"result\":{\"action\":\"accept\",\"content\":{\"color\":\"blue\"}}}\n"
    );
    exit(session);
}

#[test]
fn the_shell_opens_a_consented_url_with_the_platform_launcher() {
    let server = provider();
    let home = Home::with(&server, "yolo", CONNECT_URL);
    let session = shell(&home);
    let screen = wait(
        &session,
        "Open it? oh-fx will not fetch this URL or see browser contents.",
    );
    for line in [
        "  MCP server fixture requests an external browser action. Target host: example.test\n",
        "  Complete URL: https://example.test/connect?state=opaque\n",
        "    1) Open URL",
        "    2) Decline",
    ] {
        assert!(screen.contains(line), "{line}\n{screen}");
    }
    session.send(b"1");
    wait(&session, "done");
    assert_eq!(
        home.state("opened"),
        "https://example.test/connect?state=opaque\n"
    );
    assert_eq!(
        home.state("answer"),
        "{\"jsonrpc\":\"2.0\",\"id\":\"ask-1\",\"result\":{\"action\":\"accept\"}}\n"
    );
    exit(session);
}

#[test]
fn a_terminal_ask_declines_a_url_without_opening_it() {
    let server = provider();
    let home = Home::with(&server, "ask", CONNECT_URL);
    let mut session = PtySession::spawn(home.command(), 40, 200).expect("spawn oh-fx in a pty");
    let screen = wait(&session, "Choose a number or enter a value:");
    assert!(
        screen.contains("Complete URL: https://example.test/connect?state=opaque\nOpen it? oh-fx will not fetch this URL or see browser contents.\n  1. Open URL\n  2. Decline\nChoose a number or enter a value:"),
        "{screen}"
    );
    session.send(b"2\r");
    let status = session.wait_exit(WAIT).and_then(|status| status.code());
    assert_eq!(status, Some(0), "{}", session.screen());
    assert_eq!(
        home.state("answer"),
        "{\"jsonrpc\":\"2.0\",\"id\":\"ask-1\",\"result\":{\"action\":\"decline\"}}\n"
    );
    assert!(!home.state.join("opened").exists());
}

#[test]
fn an_ask_that_cannot_ask_for_a_url_ends_the_turn_as_mcp_input_required() {
    let server = provider();
    let home = Home::with(
        &server,
        "ask",
        r#"{"elicitations":[{"mode":"url","message":"Connect your account","url":"https://example.test/connect?state=opaque","elicitationId":"connect-1"}]}"#,
    );
    fs::write(home.state.join("url_required"), "").expect("ask for a URL first");
    let output = home
        .command()
        .arg("--json")
        .stdin(Stdio::null())
        .output()
        .expect("run oh-fx");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).expect("a JSON result");
    assert_eq!(result["error"], "McpInputRequired", "{result}");
    assert_eq!(result["exit_code"], 1);
    assert_eq!(result["final_output"], "");
    assert_eq!(
        result["tool_calls"][1],
        json!({"name": "mcp_fixture_profile", "status": "error"})
    );
    assert_eq!(
        server.requests().len(),
        2,
        "no request follows the input-required result"
    );
}

#[test]
fn the_shell_retries_a_tool_call_after_its_url_flow_is_completed() {
    let server = provider();
    let home = Home::with(
        &server,
        "yolo",
        r#"{"elicitations":[{"mode":"url","message":"Connect your account","url":"https://example.test/connect?state=opaque","elicitationId":"connect-1"}]}"#,
    );
    fs::write(home.state.join("url_required"), "").expect("ask for a URL first");
    let session = shell(&home);
    wait(
        &session,
        "Open it? oh-fx will not fetch this URL or see browser contents.",
    );
    session.send(b"1");
    let screen = wait(
        &session,
        "Otherwise choose I completed it / Retry, or Cancel.",
    );
    for line in ["    1) I completed it / Retry", "    2) Cancel"] {
        assert!(screen.contains(line), "{line}\n{screen}");
    }
    assert_eq!(
        home.state("opened"),
        "https://example.test/connect?state=opaque\n"
    );
    session.send(b"1");
    wait(&session, "done");
    let requests = server.requests();
    assert_eq!(
        last_tool_result(&requests[2]),
        r#"{"server":"fixture","tool":"profile","result":{"content":[{"type":"text","text":"answered"}]}}"#
    );
    exit(session);
}

#[test]
fn a_terminal_ask_retries_at_once_when_the_server_confirms_the_url_flow() {
    let server = provider();
    let home = Home::with(
        &server,
        "ask",
        r#"{"elicitations":[{"mode":"url","message":"Connect your account","url":"https://example.test/connect?state=opaque","elicitationId":"connect-1"}]}"#,
    );
    fs::write(home.state.join("url_required"), "").expect("ask for a URL first");
    fs::write(home.state.join("notify"), "").expect("confirm the URL flow");
    let mut session = PtySession::spawn(home.command(), 40, 200).expect("spawn oh-fx in a pty");
    wait(&session, "Choose a number or enter a value:");
    session.send(b"1\r");
    let status = session.wait_exit(WAIT).and_then(|status| status.code());
    assert_eq!(status, Some(0), "{}", session.screen());
    assert!(!session.screen().contains("Complete the browser flow"));
    assert_eq!(
        home.state("opened"),
        "https://example.test/connect?state=opaque\n"
    );
    let requests = server.requests();
    assert_eq!(
        last_tool_result(&requests[2]),
        r#"{"server":"fixture","tool":"profile","result":{"content":[{"type":"text","text":"answered"}]}}"#
    );
}
