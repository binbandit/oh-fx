use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use tokio::time::Instant;

use super::{Lifecycle, Server};
use crate::error::McpError;
use crate::health::AuthenticationState;
use crate::mcp_auth::Challenge;
use crate::mcp_contract::{McpServerConfig, TransportType};
use crate::server_connection::McpClient;
use crate::server_transport::{ConnectOptions, startup_deadline};
use crate::test_support::{FakeServer, Reply};
use crate::tool_operations::CallOptions;

#[tokio::test]
async fn a_rejection_on_a_replaced_connection_leaves_the_live_one_alone() {
    let server = FakeServer::start(|request| {
        let Some(id) = request.request_id() else {
            return Reply::status(202);
        };
        match request.method_name().as_deref() {
            Some("initialize") => Reply::json(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"remote","version":"1"}}}}}}"#
            )),
            Some("tools/list") => Reply::json(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}"#
            )),
            _ => Reply::status(401).header("WWW-Authenticate", r#"Bearer scope="tools.call""#),
        }
    })
    .await;
    let config = McpServerConfig {
        startup_timeout_ms: 5_000,
        ..McpServerConfig::remote("remote", TransportType::Http, &server.url)
    };
    let live = Arc::new(Server::new(
        config.clone(),
        ConnectOptions::default(),
        Arc::new(AtomicU64::new(0)),
    ));
    live.start().await.unwrap();
    let stale = McpClient::connect_until(
        &config,
        &ConnectOptions::default(),
        startup_deadline(&config),
        &live.auth,
    )
    .await
    .ok()
    .unwrap();
    let rejected = stale
        .call_tool(
            "echo",
            "{}",
            CallOptions::default(),
            Instant::now() + Duration::from_secs(5),
        )
        .await;
    assert_eq!(rejected.err(), Some(McpError::McpAuthenticationRequired));
    live.authentication_failed(&stale);
    assert!(matches!(live.lifecycle(), Lifecycle::Ready(_)));
    assert_eq!(live.auth.authentication(&config), AuthenticationState::None);
    assert_eq!(live.auth.pending(), Challenge::default());
}

#[tokio::test]
async fn a_failed_reconnect_leaves_the_server_as_it_was() {
    let refused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", refused.local_addr().unwrap());
    drop(refused);
    let config = McpServerConfig {
        startup_timeout_ms: 5_000,
        ..McpServerConfig::remote("remote", TransportType::Http, &url)
    };
    let server = Arc::new(Server::new(
        config,
        ConnectOptions::default(),
        Arc::new(AtomicU64::new(0)),
    ));
    assert!(server.start().await.is_err());
    let Lifecycle::Failed(before) = server.lifecycle() else {
        panic!("the server failed to start");
    };
    let restarts = server.restarts();
    assert!(server.reconnect().await.is_err());
    assert!(matches!(server.lifecycle(), Lifecycle::Failed(after) if after == before));
    assert_eq!(server.restarts(), restarts);
}

const LIST_CHANGING_SERVER: &str = r#"reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      reply "$id" '{"protocolVersion":"2025-11-25","capabilities":{"tools":{},"resources":{"listChanged":true},"prompts":{"listChanged":true}},"serverInfo":{"name":"fixture","version":"1"}}' ;;
    *'"method":"tools/list"'*)
      reply "$id" '{"tools":[{"name":"ask","inputSchema":{"type":"object"}}]}' ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","method":"notifications/resources/list_changed"}\n'
      printf '{"jsonrpc":"2.0","method":"notifications/prompts/list_changed"}\n'
      printf '{"jsonrpc":"2.0","id":"e","method":"elicitation/create","params":{"message":"Who?","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}}\n'
      IFS= read -r answer
      reply "$id" "{\"content\":[],\"structuredContent\":$answer}" ;;
  esac
done
"#;

#[tokio::test]
async fn a_feature_list_change_during_a_question_keeps_its_answer() {
    let answers = crate::test_support::ScriptedAnswers::new(&["Ada", "Submit"]);
    let options = ConnectOptions {
        elicitation: Some(crate::test_support::operation_elicitation(&answers).responder),
        ..ConnectOptions::default()
    };
    let server = Arc::new(Server::new(
        McpServerConfig::stdio(
            "fixture",
            "/bin/sh",
            vec!["-c".to_owned(), LIST_CHANGING_SERVER.to_owned()],
        ),
        options,
        Arc::new(AtomicU64::new(0)),
    ));
    server.start().await.unwrap();
    let Lifecycle::Ready(client) = server.lifecycle() else {
        panic!("the fixture is ready");
    };
    let advertised = super::Advertised {
        tool: client.tool_catalog().get("ask").unwrap().clone(),
        instructions: None,
    };
    let Ok(crate::features::tools::ToolCallOutcome::Complete(result)) =
        server.call(&advertised, "{}", CallOptions::default()).await
    else {
        panic!("the call completes");
    };
    assert_eq!(
        result.structured_content.unwrap()["result"],
        serde_json::json!({"action":"accept","content":{"name":"Ada"}})
    );
    server.stop(crate::transport::ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_logout_makes_an_open_elicitation_stale() {
    use crate::legacy_elicitation_runtime::ElicitationOwner;
    let server = Arc::new(Server::new(
        McpServerConfig::stdio(
            "fixture",
            "/bin/sh",
            vec!["-c".to_owned(), LIST_CHANGING_SERVER.to_owned()],
        ),
        ConnectOptions::default(),
        Arc::new(AtomicU64::new(0)),
    ));
    server.start().await.unwrap();
    let Lifecycle::Ready(client) = server.lifecycle() else {
        panic!("the fixture is ready");
    };
    let owner = super::CallOwner::new(&server, &client);
    assert!(owner.current());
    let detached = server.auth.begin_logout().await.unwrap();
    assert!(!owner.current());
    server.auth.restore_after_logout(detached);
    assert!(owner.current());
    let detached = server.auth.begin_logout().await.unwrap();
    server.auth.commit_logout();
    server.auth.finish_logout();
    drop(detached);
    assert!(!owner.current());
    server.stop(crate::transport::ShutdownMode::Immediate).await;
}
