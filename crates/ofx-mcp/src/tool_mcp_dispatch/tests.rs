use ofx_config::{ContextLimits, parse_context_limit_override};
use ofx_contract::{PathAccess, ToolCallId, ToolResultStatus};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::mcp_contract::McpServerConfig;
use crate::native_config::NativeConfigLoad;
use crate::server_transport::ConnectOptions;
use crate::startup_admission::StartupPhase;

const SERVER: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"},\"instructions\":\"Prefer echo.\nKeep it short.\"}" ;;
    *'"method":"tools/list"'*)
      reply "$id" '{"tools":[{"name":"echo","description":"Echo text.","inputSchema":{"type":"object"}},{"name":"wide","description":"A wide tool.","inputSchema":{"type":"object","properties":{"padding":{"description":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}}}]}' ;;
  esac
done
"#;

async fn runtime(overrides: &[&str]) -> Arc<McpRuntime> {
    let config = McpServerConfig::stdio(
        "fixture",
        "/bin/sh",
        vec!["-c".to_owned(), SERVER.to_owned()],
    );
    let mut limits = ContextLimits::default();
    let overrides: Vec<_> = overrides
        .iter()
        .map(|text| parse_context_limit_override(text.as_bytes()).unwrap())
        .collect();
    limits.apply_command_line(&overrides);
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![config],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        limits,
    ));
    runtime.connect(StartupPhase::All).await;
    runtime
}

async fn run(tool: &McpSelectTool, arguments: &str) -> ToolOutput {
    tool.prepare(arguments)
        .unwrap()
        .execute(ToolContext::new(
            ToolCallId::new("select"),
            CancellationToken::new(),
            PathAccess::WorkspaceOnly,
        ))
        .await
}

#[test]
fn spec_matches_the_upstream_golden() {
    let tool = McpSelectTool::new(None);
    let spec = tool.spec();
    let actual = format!(
        r#"{{"type":"function","name":{},"description":{},"inputSchema":{}}}"#,
        Value::from(spec.name.as_str()),
        Value::from(spec.description.as_str()),
        spec.input_schema
    );
    assert_eq!(
        actual.as_bytes(),
        include_bytes!("../../../../parity/goldens/mcp_select_tool.json")
    );
}

#[tokio::test]
async fn malformed_arguments_and_a_missing_runtime_fail_with_upstreams_messages() {
    let tool = McpSelectTool::new(None);
    for (arguments, expected) in [
        ("{", INVALID),
        ("[]", INVALID),
        (r#"{"name":"a","name":"b"}"#, INVALID),
        ("{}", NAME_REQUIRED),
        (r#"{"name":7}"#, NAME_REQUIRED),
        (r#"{"name":"mcp_fixture_echo"}"#, NO_RUNTIME),
    ] {
        let output = run(&tool, arguments).await;
        assert_eq!(output.status, ToolResultStatus::Failure);
        assert_eq!(output.content, expected, "{arguments}");
        assert!(output.selected_tools().is_empty());
    }
}

#[tokio::test]
async fn selecting_a_listed_tool_loads_it_for_the_next_step() {
    let runtime = runtime(&["mcp_server_instructions_bytes=13"]).await;
    let tool = McpSelectTool::new(Some(runtime));
    let output = run(&tool, r#"{"name":"mcp_fixture_echo"}"#).await;
    assert_eq!(output.status, ToolResultStatus::Success);
    assert_eq!(
        output.content,
        "Selected dynamic MCP tool `mcp_fixture_echo`. Its executable schema will be available on the next model step; call `mcp_fixture_echo` with arguments matching the selected schema."
    );
    assert_eq!(output.selected_tools(), ["mcp_fixture_echo"]);
    assert_eq!(
        output.context_notices,
        [
            "[context] MCP schema \"mcp_fixture_echo\" instructions truncated: observed=27 bytes effective=13 bytes source=command line; override with --context-limit mcp_server_instructions_bytes=BYTES|off"
        ]
    );
    let missing = run(&tool, r#"{"name":"echo"}"#).await;
    assert_eq!(missing.status, ToolResultStatus::Failure);
    assert_eq!(
        missing.content,
        "Dynamic MCP tool not found or not allowed: echo"
    );
    assert!(missing.selected_tools().is_empty());
}

#[tokio::test]
async fn a_schema_over_its_limit_is_refused_with_a_context_limit_rejection() {
    let runtime = runtime(&["mcp_selected_schema_bytes=200"]).await;
    let tool = McpSelectTool::new(Some(runtime));
    let output = run(&tool, r#"{"name":"mcp_fixture_wide"}"#).await;
    assert_eq!(output.status, ToolResultStatus::Failure);
    let rejection: Value = serde_json::from_str(&output.content).unwrap();
    let observed = rejection["context_limit_rejection"]["observed_bytes"]
        .as_u64()
        .unwrap();
    assert!(observed > 200);
    assert_eq!(
        output.content,
        format!(
            r#"{{"context_limit_rejection":{{"name":"mcp_selected_schema_bytes","tool":"mcp_fixture_wide","action":"rejected","observed_bytes":{observed},"effective_bytes":200,"source":"command line","override":"--context-limit mcp_selected_schema_bytes=BYTES|off"}}}}"#
        )
    );
    assert_eq!(
        output.context_notices,
        [format!(
            "[context] MCP schema \"mcp_fixture_wide\" rejected: observed={observed} bytes effective=200 bytes source=command line; override with --context-limit mcp_selected_schema_bytes=BYTES|off"
        )]
    );
    assert!(output.selected_tools().is_empty());
}

#[test]
fn calls_present_the_selected_name() {
    let tool = McpSelectTool::new(None);
    for (arguments, title) in [
        (
            r#"{"name":"mcp_fixture_echo"}"#,
            "Selecting MCP tool mcp_fixture_echo",
        ),
        ("{}", "Selecting MCP tool dynamic tool"),
        ("[]", "Working: mcp_select_tool"),
    ] {
        let description = tool.prepare(arguments).unwrap().describe();
        assert_eq!(description.title, title);
        assert_eq!(description.activity, ToolActivity::Read);
        assert_eq!(description.concurrency, Concurrency::Serial);
    }
}

const EXPIRING: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" "$(cat "$STATE/tools.json")" ;;
  esac
done
"#;

#[tokio::test]
async fn selecting_lists_the_owning_servers_expired_tool_list_again_first() {
    let state = tempfile::tempdir().unwrap();
    let tools = |padding: usize, extra: &[&str]| {
        let mut listed = vec![serde_json::json!({
            "name": "echo",
            "inputSchema": {"type": "object", "properties": {"text": {"description": "x".repeat(padding)}}}
        })];
        listed.extend(
            extra
                .iter()
                .map(|name| serde_json::json!({"name": name, "inputSchema": {"type": "object"}})),
        );
        serde_json::json!({"tools": listed, "ttlMs": 1}).to_string()
    };
    std::fs::write(state.path().join("tools.json"), tools(1, &[])).unwrap();
    let mut config = McpServerConfig::stdio(
        "fixture",
        "/bin/sh",
        vec!["-c".to_owned(), EXPIRING.to_owned()],
    );
    config.env.push(crate::mcp_contract::EnvVar {
        key: "STATE".to_owned(),
        value: state.path().to_string_lossy().into_owned(),
    });
    let mut limits = ContextLimits::default();
    limits.apply_command_line(&[
        parse_context_limit_override(b"mcp_selected_schema_bytes=300").unwrap(),
    ]);
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![config],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        limits,
    ));
    runtime.connect(StartupPhase::All).await;
    std::fs::write(state.path().join("tools.json"), tools(400, &["later"])).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let tool = McpSelectTool::new(Some(runtime));
    let unlisted = run(&tool, r#"{"name":"mcp_fixture_later"}"#).await;
    assert_eq!(
        unlisted.content,
        "Dynamic MCP tool not found or not allowed: mcp_fixture_later"
    );
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let refreshed = run(&tool, r#"{"name":"mcp_fixture_echo"}"#).await;
    assert_eq!(refreshed.status, ToolResultStatus::Failure);
    assert!(
        refreshed
            .content
            .starts_with(r#"{"context_limit_rejection":{"name":"mcp_selected_schema_bytes","tool":"mcp_fixture_echo""#),
        "{}",
        refreshed.content
    );
}

const STALLING: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*)
      if [ -f "$STATE/listed" ]; then touch "$STATE/stalled"; cat > /dev/null; exit 0; fi
      touch "$STATE/listed"
      reply "$id" '{"tools":[{"name":"echo","inputSchema":{"type":"object"}}],"ttlMs":1}' ;;
  esac
done
"#;

#[tokio::test]
async fn cancelling_the_turn_ends_a_selection_waiting_on_a_stalled_tool_list() {
    let state = tempfile::tempdir().unwrap();
    let mut config = McpServerConfig::stdio(
        "fixture",
        "/bin/sh",
        vec!["-c".to_owned(), STALLING.to_owned()],
    );
    config.env.push(crate::mcp_contract::EnvVar {
        key: "STATE".to_owned(),
        value: state.path().to_string_lossy().into_owned(),
    });
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![config],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    ));
    runtime.connect(StartupPhase::All).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let cancellation = CancellationToken::new();
    let selecting = tokio::spawn(
        McpSelectTool::new(Some(runtime))
            .prepare(r#"{"name":"mcp_fixture_echo"}"#)
            .unwrap()
            .execute(ToolContext::new(
                ToolCallId::new("select"),
                cancellation.clone(),
                PathAccess::WorkspaceOnly,
            )),
    );
    let stalled = state.path().join("stalled");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !stalled.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    let output = tokio::time::timeout(std::time::Duration::from_secs(1), selecting)
        .await
        .expect("the selection ends once cancelled")
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert_eq!(
        output.content,
        format_tool_execution_error_json(NAME, "Cancelled")
    );
}

#[tokio::test]
async fn ask_starts_the_server_a_selected_name_belongs_to() {
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![McpServerConfig::stdio(
                "fixture",
                "/bin/sh",
                vec!["-c".to_owned(), SERVER.to_owned()],
            )],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    ));
    runtime.connect_for_ask(false).await;
    let tool = McpSelectTool::new(Some(Arc::clone(&runtime)));
    let output = run(&tool, r#"{"name":"mcp_fixture_echo"}"#).await;
    assert_eq!(
        output.status,
        ToolResultStatus::Success,
        "{}",
        output.content
    );
    assert_eq!(output.selected_tools(), ["mcp_fixture_echo"]);
}

#[tokio::test]
async fn ask_reports_why_the_server_a_selected_name_belongs_to_failed_to_start() {
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![McpServerConfig::stdio(
                "broken",
                "/bin/sh",
                vec!["-c".to_owned(), "exit 3".to_owned()],
            )],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    ));
    runtime.connect_for_ask(false).await;
    let tool = McpSelectTool::new(Some(Arc::clone(&runtime)));
    let output = run(&tool, r#"{"name":"mcp_broken_echo"}"#).await;
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert_eq!(
        output.content,
        format_tool_execution_error_json(NAME, "McpServerExitedDuringStartup")
    );
    let again = run(&tool, r#"{"name":"mcp_broken_echo"}"#).await;
    assert_eq!(
        again.content,
        "Dynamic MCP tool not found or not allowed: mcp_broken_echo"
    );
}

const HELD: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      touch "$STATE/initializing"
      while [ -f "$STATE/hold" ]; do sleep 0.05; done
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*)
      reply "$id" '{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}' ;;
  esac
done
"#;

fn held(state: &std::path::Path, startup_timeout_ms: u32) -> Arc<McpRuntime> {
    let mut config =
        McpServerConfig::stdio("fixture", "/bin/sh", vec!["-c".to_owned(), HELD.to_owned()]);
    config.startup_timeout_ms = startup_timeout_ms;
    config.env.push(crate::mcp_contract::EnvVar {
        key: "STATE".to_owned(),
        value: state.to_string_lossy().into_owned(),
    });
    Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![config],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    ))
}

#[tokio::test]
async fn a_selection_cancelled_while_its_server_starts_records_the_start_as_cancelled() {
    let state = tempfile::tempdir().unwrap();
    std::fs::write(state.path().join("hold"), "").unwrap();
    let runtime = held(state.path(), 30_000);
    runtime.connect_for_ask(false).await;
    let tool = McpSelectTool::new(Some(Arc::clone(&runtime)));
    let cancellation = CancellationToken::new();
    let selecting = tokio::spawn(
        tool.prepare(r#"{"name":"mcp_fixture_echo"}"#)
            .unwrap()
            .execute(ToolContext::new(
                ToolCallId::new("select"),
                cancellation.clone(),
                PathAccess::WorkspaceOnly,
            )),
    );
    let initializing = state.path().join("initializing");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !initializing.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    assert_eq!(
        selecting.await.unwrap().content,
        format_tool_execution_error_json(NAME, "Cancelled")
    );
    assert!(matches!(
        runtime.current()[0].lifecycle(),
        crate::server_lifecycle::Lifecycle::Failed(failure) if failure == "Cancelled"
    ));
    std::fs::remove_file(state.path().join("hold")).unwrap();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run(&tool, r#"{"name":"mcp_fixture_echo"}"#),
    )
    .await
    .expect("the next selection answers at once");
    assert_eq!(
        output.content,
        "Dynamic MCP tool not found or not allowed: mcp_fixture_echo"
    );
}

#[tokio::test]
async fn a_server_that_misses_its_startup_timeout_fails_the_selection_as_upstream_names_it() {
    let state = tempfile::tempdir().unwrap();
    std::fs::write(state.path().join("hold"), "").unwrap();
    let runtime = held(state.path(), 200);
    runtime.connect_for_ask(false).await;
    let output = run(
        &McpSelectTool::new(Some(Arc::clone(&runtime))),
        r#"{"name":"mcp_fixture_echo"}"#,
    )
    .await;
    assert_eq!(
        output.content,
        format_tool_execution_error_json(NAME, "McpConnectionTimedOut")
    );
}

fn remote_runtime(url: &str) -> Arc<McpRuntime> {
    Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![McpServerConfig::remote(
                "fixture",
                crate::mcp_contract::TransportType::Http,
                url,
            )],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    ))
}

async fn cancel_when(runtime: &Arc<McpRuntime>, started: impl Future<Output = ()>) -> ToolOutput {
    let cancellation = CancellationToken::new();
    let selecting = tokio::spawn(
        McpSelectTool::new(Some(Arc::clone(runtime)))
            .prepare(r#"{"name":"mcp_fixture_echo"}"#)
            .unwrap()
            .execute(ToolContext::new(
                ToolCallId::new("select"),
                cancellation.clone(),
                PathAccess::WorkspaceOnly,
            )),
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), started)
        .await
        .unwrap();
    cancellation.cancel();
    selecting.await.unwrap()
}

#[tokio::test]
async fn a_start_cancelled_while_an_http_server_lists_its_tools_ends_the_session() {
    use crate::test_support::{FakeServer, Reply};
    let server = FakeServer::start(|request| match request.method.as_str() {
        "DELETE" => Reply::status(204),
        _ => match request.method_name().as_deref() {
            Some("initialize") => Reply::json(&format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fixture","version":"1"}}}}}}"#,
                request.request_id().unwrap()
            ))
            .header("Mcp-Session-Id", "session-1"),
            Some("tools/list") => Reply::sse(&[]).held_open(),
            _ => Reply::status(202),
        },
    })
    .await;
    let runtime = remote_runtime(&server.url);
    runtime.connect_for_ask(false).await;
    let output = cancel_when(&runtime, async {
        assert!(
            server
                .wait_for(|request| request.method_name().as_deref() == Some("tools/list"))
                .await
        );
    })
    .await;
    assert_eq!(
        output.content,
        format_tool_execution_error_json(NAME, "Cancelled")
    );
    assert!(
        server
            .wait_for(|request| request.method == "DELETE"
                && request.header("mcp-session-id") == Some("session-1"))
            .await
    );
}

const STALLED_LISTING: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*)
      echo $$ > "$STATE/pid"
      while :; do sleep 1; done ;;
  esac
done
"#;

fn still_running(pid: &str) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .is_ok_and(|output| {
            let state = String::from_utf8_lossy(&output.stdout);
            let state = state.trim();
            !state.is_empty() && !state.starts_with('Z')
        })
}

#[tokio::test]
async fn a_start_cancelled_while_a_stdio_server_lists_its_tools_stops_the_server() {
    let state = tempfile::tempdir().unwrap();
    let mut config = McpServerConfig::stdio(
        "fixture",
        "/bin/sh",
        vec!["-c".to_owned(), STALLED_LISTING.to_owned()],
    );
    config.env.push(crate::mcp_contract::EnvVar {
        key: "STATE".to_owned(),
        value: state.path().to_string_lossy().into_owned(),
    });
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![config],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    ));
    runtime.connect_for_ask(false).await;
    let recorded = state.path().join("pid");
    let output = cancel_when(&runtime, async {
        while std::fs::read_to_string(&recorded).map_or(true, |pid| pid.trim().is_empty()) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_eq!(
        output.content,
        format_tool_execution_error_json(NAME, "Cancelled")
    );
    let pid = std::fs::read_to_string(&recorded)
        .unwrap()
        .trim()
        .to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while still_running(&pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        !still_running(&pid),
        "server {pid} outlived its cancelled start"
    );
}
