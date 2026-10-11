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

const URL_REQUIRED_SERVER: &str = r#"reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
calls=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      reply "$id" '{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"fixture","version":"1"}}' ;;
    *'"method":"tools/list"'*)
      if [ "$calls" -gt 0 ] && [ "$CHANGE" = 1 ]; then
        reply "$id" '{"tools":[{"name":"connect","description":"changed","inputSchema":{"type":"object"}}]}'
      else
        reply "$id" '{"tools":[{"name":"connect","inputSchema":{"type":"object"}}]}'
      fi ;;
    *'"method":"tools/call"'*)
      calls=$((calls + 1))
      if [ "$calls" -le "${ROUNDS:-1}" ]; then
        if [ "$CHANGE" = 1 ]; then
          printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
        fi
        if [ "$NOTIFY" = 1 ]; then
          printf '{"jsonrpc":"2.0","method":"notifications/elicitation/complete","params":{"elicitationId":"e1"}}\n'
        fi
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32042,"message":"URL required","data":{"elicitations":[{"mode":"url","message":"Connect","url":"https://example.test/connect","elicitationId":"e1"}]}}}\n' "$id"
      else
        reply "$id" '{"content":[{"type":"text","text":"connected"}]}'
      fi ;;
  esac
done
"#;

async fn connect_through_url(
    notify: bool,
    answers: Option<&Arc<crate::test_support::ScriptedAnswers>>,
) -> crate::features::tools::ToolCallOutcome {
    connect_with(&[("NOTIFY", if notify { "1" } else { "0" })], answers).await
}

async fn connect_with(
    environment: &[(&str, &str)],
    answers: Option<&Arc<crate::test_support::ScriptedAnswers>>,
) -> crate::features::tools::ToolCallOutcome {
    let options = ConnectOptions {
        elicitation: answers
            .map(|answers| crate::test_support::operation_elicitation(answers).responder),
        ..ConnectOptions::default()
    };
    let mut config = McpServerConfig::stdio(
        "fixture",
        "/bin/sh",
        vec!["-c".to_owned(), URL_REQUIRED_SERVER.to_owned()],
    );
    config.env.extend(
        environment
            .iter()
            .map(|(key, value)| crate::mcp_contract::EnvVar {
                key: (*key).to_owned(),
                value: (*value).to_owned(),
            }),
    );
    let server = Arc::new(Server::new(config, options, Arc::new(AtomicU64::new(0))));
    server.start().await.unwrap();
    let Lifecycle::Ready(client) = server.lifecycle() else {
        panic!("the fixture is ready");
    };
    let advertised = super::Advertised {
        tool: client.tool_catalog().get("connect").unwrap().clone(),
        instructions: None,
    };
    let outcome = server.call(&advertised, "{}", CallOptions::default()).await;
    server.stop(crate::transport::ShutdownMode::Immediate).await;
    let Ok(outcome) = outcome else {
        panic!("the call returns an outcome");
    };
    outcome
}

fn connected(outcome: &crate::features::tools::ToolCallOutcome) -> bool {
    matches!(outcome, crate::features::tools::ToolCallOutcome::Complete(result) if result.result["content"][0]["text"] == "connected")
}

#[tokio::test]
async fn a_completed_url_elicitation_retries_the_call_without_asking_again() {
    let answers = crate::test_support::ScriptedAnswers::new(&["Open URL"]);
    let outcome = connect_through_url(true, Some(&answers)).await;
    assert!(connected(&outcome));
    assert_eq!(answers.questions().len(), 1);
    assert!(answers.questions()[0].starts_with(
        "MCP server fixture requests an external browser action. Target host: example.test\nComplete URL: https://example.test/connect"
    ));
}

#[tokio::test]
async fn an_unconfirmed_url_elicitation_retries_once_the_user_says_so() {
    let answers =
        crate::test_support::ScriptedAnswers::new(&["Open URL", "I completed it / Retry"]);
    let outcome = connect_through_url(false, Some(&answers)).await;
    assert!(connected(&outcome));
    assert_eq!(answers.questions().len(), 2);
}

#[tokio::test]
async fn a_declined_url_elicitation_ends_the_call_and_without_a_responder_it_is_returned() {
    let answers = crate::test_support::ScriptedAnswers::new(&["Decline"]);
    assert!(matches!(
        connect_through_url(true, Some(&answers)).await,
        crate::features::tools::ToolCallOutcome::UrlElicitationEnded(
            "MCP URL elicitation was declined"
        )
    ));
    let cancelled = crate::test_support::ScriptedAnswers::new(&["Open URL", "Cancel"]);
    assert!(matches!(
        connect_through_url(false, Some(&cancelled)).await,
        crate::features::tools::ToolCallOutcome::UrlElicitationEnded(
            "MCP URL elicitation was cancelled"
        )
    ));
    let crate::features::tools::ToolCallOutcome::InputRequired(requests) =
        connect_through_url(true, None).await
    else {
        panic!("the URL-required result is returned");
    };
    assert_eq!(
        requests,
        r#"{"e1":{"method":"elicitation/create","params":{"mode":"url","message":"Connect","url":"https://example.test/connect","elicitationId":"e1"}}}"#
    );
}

#[tokio::test]
async fn a_url_elicitation_is_retried_at_most_eight_times() {
    let answers = crate::test_support::ScriptedAnswers::new(&["Open URL"; 8]);
    assert!(connected(
        &connect_with(&[("NOTIFY", "1"), ("ROUNDS", "8")], Some(&answers)).await
    ));
    assert_eq!(answers.questions().len(), 8);
    let answers = crate::test_support::ScriptedAnswers::new(&["Open URL"; 8]);
    assert!(matches!(
        connect_with(&[("NOTIFY", "1"), ("ROUNDS", "9")], Some(&answers)).await,
        crate::features::tools::ToolCallOutcome::InputRoundsExceeded
    ));
    assert_eq!(answers.questions().len(), 8);
}

#[tokio::test]
async fn a_tool_list_change_during_the_url_flow_cancels_the_retry() {
    let answers = crate::test_support::ScriptedAnswers::new(&["Open URL"]);
    assert!(matches!(
        connect_with(&[("NOTIFY", "1"), ("CHANGE", "1")], Some(&answers)).await,
        crate::features::tools::ToolCallOutcome::UrlElicitationEnded(
            "MCP URL elicitation was cancelled"
        )
    ));
    assert_eq!(answers.questions().len(), 1);
}
