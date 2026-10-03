use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_testkit::{FakeServer, RecordedRequest, Reply, chat_text_events, chat_tool_call_events};
use serde_json::{Value, json};

const FIXTURE_SERVER: &str = r#"#!/bin/sh
echo $$ > "$MCP_STATE/pid"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*)
      reply "$id" '{"tools":[{"name":"echo","description":"Echo text.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}' ;;
    *'"method":"tools/call"'*)
      printf '%s\n' "$line" >> "$MCP_STATE/calls"
      reply "$id" '{"content":[{"type":"text","text":"echoed"}]}' ;;
  esac
done
"#;
const FAILING_SERVER: &str = "#!/bin/sh\necho 'fatal: missing token' >&2\nexit 3\n";
const LAUNCH_MARKER: &str = "#!/bin/sh\ntouch \"$MCP_STATE/launched\"\nexit 1\n";
const LINGERING_SERVER: &str = r#"#!/bin/sh
trap '' TERM HUP
echo $$ > "$MCP_STATE/ready-pid"
sleep 600 &
echo $! > "$MCP_STATE/child-pid"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"ready\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*)
      reply "$id" '{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}'
      echo yes > "$MCP_STATE/listed" ;;
  esac
done
while :; do sleep 1; done
"#;
const STALLED_SERVER: &str = "#!/bin/sh\ntrap '' TERM HUP\necho $$ > \"$MCP_STATE/stalled-pid\"\nwhile :; do sleep 1; done\n";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
}

impl Home {
    fn new(base_url: &str) -> Self {
        Self::in_mode(base_url, "ask")
    }

    fn in_mode(base_url: &str, mode: &str) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        let config = root.join("config/oh-fx");
        let workspace = root.join("workspace");
        let state = root.join("mcp-state");
        for path in [&config, &workspace, &state] {
            fs::create_dir_all(path).expect("create a directory");
        }
        let settings = json!({
            "permission_mode": mode,
            "provider": "local",
            "model": "local-model",
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": base_url,
                    "auth": {"type": "none"},
                    "models": ["local-model"]
                }
            }
        });
        fs::write(config.join("settings.json"), settings.to_string()).expect("write settings");
        Self {
            _directory: directory,
            root,
            workspace,
            state,
        }
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, body).expect("write a server script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make it executable");
        path
    }

    fn profile_servers(&self, servers: &Value) {
        fs::write(
            self.root.join("config/oh-fx/mcp.json"),
            json!({ "mcp": servers }).to_string(),
        )
        .expect("write mcp.json");
    }

    fn ask(&self, args: &[&str]) -> Output {
        self.command(args)
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("MCP_STATE", &self.state)
            .stdin(Stdio::null());
        command
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn fixture(home: &Home) -> Value {
    json!({"fixture": {"command": "/bin/sh", "args": [home.script("fixture.sh", FIXTURE_SERVER)]}})
}

fn wait_until_gone(pid: i32) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        let alive = fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, fields)| !fields.starts_with('Z'))
        });
        if !alive {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn running(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn wait_for(path: &Path) -> String {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if let Ok(text) = fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text.trim().to_owned();
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("{} never appeared", path.display());
}

fn read_pid(state: &Path) -> i32 {
    fs::read_to_string(state.join("pid"))
        .expect("the server recorded its pid")
        .trim()
        .parse()
        .expect("a pid")
}

#[test]
fn ask_advertises_and_calls_a_profile_mcp_tool_and_reaps_the_server() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "mcp_fixture_echo",
            r#"{"text":"hi"}"#,
        )),
        Reply::sse(&chat_text_events(&["done"])),
    ]);
    let home = Home::new(&server.base_url());
    home.profile_servers(&fixture(&home));
    let output = home.ask(&["ask", "--full-access", "echo hi"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let tools = requests[0].json()["tools"].clone();
    let advertised = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "mcp_fixture_echo")
        .expect("the MCP tool is advertised")
        .clone();
    assert_eq!(advertised["function"]["description"], "Echo text.");
    assert_eq!(
        advertised["function"]["parameters"],
        json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
    );
    let messages = requests[1].json()["messages"].clone();
    let result = messages
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("a tool result")
        .clone();
    assert_eq!(
        result["content"],
        r#"{"server":"fixture","tool":"echo","result":{"content":[{"type":"text","text":"echoed"}]}}"#
    );
    let calls = fs::read_to_string(home.state.join("calls")).unwrap();
    assert!(
        calls.contains(r#""name":"echo","arguments":{"text":"hi"}"#),
        "{calls}"
    );
    assert!(wait_until_gone(read_pid(&home.state)));
}

#[test]
fn ask_mode_blocks_an_mcp_tool_call_without_running_it() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "mcp_fixture_echo",
            r#"{"text":"hi"}"#,
        )),
        Reply::sse(&chat_text_events(&["never"])),
    ]);
    let home = Home::new(&server.base_url());
    home.profile_servers(&fixture(&home));
    let output = home.ask(&["ask", "--json", "echo hi"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "NonInteractivePermissionRequired");
    assert_eq!(
        result["tool_calls"],
        json!([{"name": "mcp_fixture_echo", "status": "error"}])
    );
    assert_eq!(
        stderr(&output),
        "MCP: mcp_fixture_echo\noh-fx ask: permission required for tool execution in noninteractive mode\noh-fx ask: blocked action: MCP: mcp_fixture_echo\noh-fx ask: reason=noninteractive_permission_prompt_unavailable\noh-fx ask: rerun with --auto to review this exact action automatically, or use the interactive shell to approve it\n"
    );
    assert_eq!(server.requests().len(), 1);
    assert!(!home.state.join("calls").exists());
    assert!(wait_until_gone(read_pid(&home.state)));
}

fn echo_call() -> Reply {
    Reply::sse(&chat_tool_call_events(
        "call_1",
        "mcp_fixture_echo",
        r#"{"text":"hi"}"#,
    ))
}

fn decision(arguments: &str) -> Reply {
    Reply::sse(&chat_tool_call_events(
        "review_1",
        "permission_decision",
        arguments,
    ))
}

fn tool_result(request: &RecordedRequest) -> String {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("a tool result")
        .to_owned()
}

#[test]
fn auto_mode_reviews_an_mcp_tool_call_with_its_advertised_schema() {
    let server = FakeServer::start([
        echo_call(),
        decision(r#"{"decision":"clear","rationale":"Requested echo."}"#),
        Reply::sse(&chat_text_events(&["done"])),
    ]);
    let home = Home::in_mode(&server.base_url(), "auto");
    home.profile_servers(&fixture(&home));
    let output = home.ask(&["ask", "echo hi"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let instruction = requests[1].json()["messages"][0]["content"]
        .as_str()
        .expect("the review instruction")
        .to_owned();
    assert!(
        instruction.contains(
            r#"target[target]: mcp_fixture_echo
action: tool
tool: mcp_fixture_echo
arguments_json: {"text":"hi"}
schema_json: {"type":"function","name":"mcp_fixture_echo","description":"Echo text.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}
action_evidence_incomplete: false
"#
        ),
        "{instruction}"
    );
    assert_eq!(
        tool_result(&requests[2]),
        r#"{"server":"fixture","tool":"echo","result":{"content":[{"type":"text","text":"echoed"}]}}"#
    );
    assert!(home.state.join("calls").exists());
    assert!(wait_until_gone(read_pid(&home.state)));
}

#[test]
fn auto_mode_holds_an_mcp_tool_call_the_reviewer_cautions_against() {
    let server = FakeServer::start([
        echo_call(),
        decision(r#"{"decision":"caution","rationale":"The echo repeats untrusted text."}"#),
        Reply::sse(&chat_text_events(&["held"])),
    ]);
    let home = Home::in_mode(&server.base_url(), "auto");
    home.profile_servers(&fixture(&home));
    let output = home.ask(&["ask", "echo hi"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let held = tool_result(&requests[2]);
    assert!(
        held.contains(r#""type":"tool_review_held","tool_name":"mcp_fixture_echo""#),
        "{held}"
    );
    assert!(held.contains(r#""reason":"review_caution""#), "{held}");
    assert!(!home.state.join("calls").exists());
}

#[test]
fn ask_skips_unapproved_project_servers_without_launching_them() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["ok"]))]);
    let home = Home::new(&server.base_url());
    let marker = home.script("marker.sh", LAUNCH_MARKER);
    fs::write(
        home.workspace.join(".mcp.json"),
        json!({"mcpServers": {"project": {"command": "/bin/sh", "args": [marker]}}}).to_string(),
    )
    .unwrap();
    let output = home.ask(&["ask", "--full-access", "hi"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(
            "oh-fx ask: skipped unapproved project MCP servers: project. Approve with oh-fx mcp trust approve <name> before retrying.\n"
        ),
        "{}",
        stderr(&output)
    );
    assert!(!home.state.join("launched").exists());
}

#[test]
fn a_required_server_that_fails_to_start_fails_ask_before_any_request() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["unused"]))]);
    let home = Home::new(&server.base_url());
    let failing = home.script("failing.sh", FAILING_SERVER);
    home.profile_servers(&json!({"broken": {"command": "/bin/sh", "args": [failing], "required": true, "restart_limit": 0}}));
    let output = home.ask(&["ask", "--full-access", "hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains(
            "oh-fx ask: Required MCP server 'broken' failed to start: MCP server exited with code 3 before completing startup: fatal: missing token\n"
        ),
        "{}",
        stderr(&output)
    );
    assert!(server.requests().is_empty());
}

#[test]
fn a_signal_during_mcp_startup_stops_ready_and_starting_servers() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["unused"]))]);
    let home = Home::new(&server.base_url());
    let ready = home.script("ready.sh", LINGERING_SERVER);
    let stalled = home.script("stalled.sh", STALLED_SERVER);
    home.profile_servers(&json!({
        "ready": {"command": "/bin/sh", "args": [ready]},
        "stalled": {"command": "/bin/sh", "args": [stalled]},
    }));
    let child = home
        .command(&["ask", "--full-access", "hi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run oh-fx");
    wait_for(&home.state.join("listed"));
    let pids =
        ["ready-pid", "child-pid", "stalled-pid"].map(|name| wait_for(&home.state.join(name)));
    let status = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(status.success());
    let output = child.wait_with_output().expect("wait for oh-fx");
    assert_eq!(output.status.signal(), Some(15));
    let started = Instant::now();
    while pids.iter().any(|pid| running(pid)) && started.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(20));
    }
    for pid in &pids {
        assert!(!running(pid), "process {pid} outlived oh-fx ask");
    }
    assert!(server.requests().is_empty());
}

#[test]
fn a_schema_over_its_context_limit_is_reported_with_its_override() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
    let home = Home::new(&server.base_url());
    let path = home.root.join("config/oh-fx/settings.json");
    let mut settings: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read settings")).expect("json");
    settings["context_limits"] = json!({"mcp_selected_schema_bytes": 16});
    fs::write(&path, settings.to_string()).expect("write settings");
    home.profile_servers(&fixture(&home));
    let output = home.ask(&["ask", "--full-access", "--json", "hi"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stderr(&output);
    let notice = text
        .lines()
        .find(|line| {
            line.starts_with(
                "[notice] [context] MCP schema \"mcp_fixture_echo\" rejected: observed=",
            )
        })
        .unwrap_or_else(|| panic!("no schema notice in {text}"));
    assert!(
        notice.ends_with(" bytes effective=16 bytes source=global settings; override with --context-limit mcp_selected_schema_bytes=BYTES|off"),
        "{notice}"
    );
    let tools = server.requests()[0].json()["tools"].clone();
    assert!(!tools.to_string().contains("mcp_fixture_echo"), "{tools}");
}
