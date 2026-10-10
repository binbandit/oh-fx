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
