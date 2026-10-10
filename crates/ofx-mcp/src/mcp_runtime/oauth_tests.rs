use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ofx_config::ContextLimits;
use ofx_contract::{
    DynamicTools, McpSearchHost, McpSearchRequest, McpToolSearch, ToolResultStatus,
};
use ofx_text::PreparedQuery;

use super::McpRuntime;
use super::tests::call;
use crate::mcp_contract::{McpServerConfig, TransportType};
use crate::model_catalog::Availability;
use crate::native_config::NativeConfigLoad;
use crate::server_lifecycle::Lifecycle;
use crate::server_transport::ConnectOptions;
use crate::startup_admission::StartupPhase;

fn remote_runtime(url: &str) -> McpRuntime {
    McpRuntime::new(
        NativeConfigLoad {
            configs: vec![McpServerConfig {
                startup_timeout_ms: 5_000,
                ..McpServerConfig::remote("remote", TransportType::Http, url)
            }],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    )
}

const CHALLENGE: &str =
    r#"Bearer realm="mcp", scope="tools.call", resource_metadata="https://auth.example/prm""#;
const GUIDANCE: &str = r#"{"tools":[],"count":0,"authentication_required":{"server":"remote","interactive":true,"message":"Run /mcp auth remote --open in an interactive oh-fx session."}}"#;
const NEEDS_AUTH: [&str; 2] = [
    "state=failed auth=required status=needs_auth\n",
    "    failure=Authentication is required or the saved credentials lack access; run /mcp auth remote --open and check server permissions.\n",
];

fn request(query: &str) -> McpSearchRequest {
    McpSearchRequest {
        query: Arc::new(PreparedQuery::prepare(query.to_owned()).unwrap()),
        server: None,
        host: McpSearchHost::Interactive,
    }
}

fn echo_server_reply(
    request: &crate::test_support::RecordedRequest,
    tools: &str,
    listing: &str,
) -> crate::test_support::Reply {
    use crate::test_support::Reply;
    let Some(id) = request.request_id() else {
        return Reply::status(202);
    };
    match request.method_name().as_deref() {
        Some("initialize") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{tools}}},"serverInfo":{{"name":"remote","version":"1"}}}}}}"#
        )),
        Some("tools/list") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]{listing}}}}}"#
        )),
        Some("tools/call") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[]}}}}"#
        )),
        _ => Reply::status(202),
    }
}

fn needs_authentication(runtime: &McpRuntime) {
    let health = runtime.render_health();
    for line in NEEDS_AUTH {
        assert!(health.contains(line), "{health}");
    }
    assert_eq!(
        runtime.model_catalog()[0].availability,
        Availability::AuthenticationRequired
    );
    let summary = runtime.render_summary();
    assert!(
        summary.contains("1 needs auth, 0 failed. Run /mcp auth remote --open."),
        "{summary}"
    );
    let searched = runtime.search(&request("remote tools"));
    assert_eq!(searched.model_output, GUIDANCE);
    let server = &runtime.current()[0];
    assert!(matches!(
        server.lifecycle(),
        Lifecycle::Failed(message)
            if message == "Authentication required. Run /mcp auth remote --open, or configure bearer_token_env."
    ));
    assert_eq!(
        server.auth.pending(),
        crate::mcp_auth::Challenge {
            resource_metadata: Some("https://auth.example/prm".to_owned()),
            scope: Some("tools.call".to_owned()),
            insufficient_scope: false,
        }
    );
}

#[tokio::test]
async fn an_initialize_answered_with_401_needs_authentication_with_its_challenge() {
    use crate::test_support::{FakeServer, Reply};
    let server =
        FakeServer::start(|_| Reply::status(401).header("WWW-Authenticate", CHALLENGE)).await;
    let runtime = remote_runtime(&server.url);
    runtime.connect(StartupPhase::All).await;
    needs_authentication(&runtime);
}

#[tokio::test]
async fn a_tool_call_answered_with_401_retires_the_server_until_it_authenticates() {
    use crate::test_support::{FakeServer, Reply};
    let server = FakeServer::start(|request| match request.method_name().as_deref() {
        Some("initialize") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"remote","version":"1"}}}}}}"#,
            request.request_id().unwrap()
        )),
        Some("tools/list") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}"#,
            request.request_id().unwrap()
        )),
        Some("tools/call") => Reply::status(401).header("WWW-Authenticate", CHALLENGE),
        _ => Reply::status(202),
    })
    .await;
    let runtime = remote_runtime(&server.url);
    runtime.connect(StartupPhase::All).await;
    assert_eq!(runtime.model_catalog()[0].availability, Availability::Ready);
    let output = call(&runtime, "mcp_remote_echo", "{}").await;
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert!(
        output.content.contains("McpAuthenticationRequired"),
        "{}",
        output.content
    );
    needs_authentication(&runtime);
    assert!(runtime.tools().is_empty());
}

fn refreshing_reply(
    request: &crate::test_support::RecordedRequest,
    rejecting: &AtomicBool,
) -> crate::test_support::Reply {
    use crate::test_support::Reply;
    if request.path == "/token" {
        return if rejecting.load(Ordering::Acquire) {
            Reply::json(r#"{"error":"invalid_grant"}"#).with_status(400)
        } else {
            Reply::json(
                r#"{"access_token":"short-token","refresh_token":"next-refresh","expires_in":1,"token_type":"Bearer"}"#,
            )
        };
    }
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
        Some("tools/call") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[]}}}}"#
        )),
        _ => Reply::status(202),
    }
}

fn save_expiring_grant(data: &Path, url: &str) {
    use crate::mcp_auth::Credentials;
    use crate::mcp_auth_store::{CredentialStore, GrantLookup};
    use zeroize::Zeroizing;
    CredentialStore::new(data)
        .save(
            &GrantLookup::new("remote", url, None, None).unwrap(),
            &Credentials {
                endpoint: url.to_owned(),
                resource: url.to_owned(),
                issuer: "https://issuer.example".to_owned(),
                client_id: "client".to_owned(),
                client_secret: None,
                access_token: Zeroizing::new("stored-token".to_owned()),
                refresh_token: Some(Zeroizing::new("stored-refresh".to_owned())),
                scope: "tools".to_owned(),
                token_type: "Bearer".to_owned(),
                token_endpoint_auth_method: "none".to_owned(),
                expires_at_ms: 1,
                authorization_endpoint: "https://issuer.example/authorize".to_owned(),
                token_endpoint: url.replace("/mcp", "/token"),
                revocation_endpoint: None,
            },
        )
        .unwrap();
}

#[tokio::test]
async fn a_grant_refresh_rejected_after_startup_fails_the_server_with_upstreams_guidance() {
    let rejecting = Arc::new(AtomicBool::new(false));
    let shared = Arc::clone(&rejecting);
    let server =
        crate::test_support::FakeServer::start(move |request| refreshing_reply(request, &shared))
            .await;
    let data_dir = tempfile::tempdir().unwrap();
    let data = std::fs::canonicalize(data_dir.path())
        .unwrap()
        .join("oh-fx");
    save_expiring_grant(&data, &server.url);
    let runtime = McpRuntime::new(
        NativeConfigLoad {
            configs: vec![McpServerConfig {
                startup_timeout_ms: 5_000,
                allow_stored_credentials: true,
                ..McpServerConfig::remote("remote", TransportType::Http, &server.url)
            }],
            ..NativeConfigLoad::default()
        },
        ConnectOptions {
            profile_data: Some(data),
            ..ConnectOptions::default()
        },
        Vec::new(),
        ContextLimits::default(),
    );
    runtime.connect(StartupPhase::All).await;
    assert_eq!(runtime.model_catalog()[0].availability, Availability::Ready);
    assert!(runtime.render_health().contains("auth=authenticated"));
    rejecting.store(true, Ordering::Release);
    let output = call(&runtime, "mcp_remote_echo", "{}").await;
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert!(
        output.content.contains("McpRefreshRejected"),
        "{}",
        output.content
    );
    let health = runtime.render_health();
    assert!(
        health.contains("state=failed auth=required status=needs_auth\n"),
        "{health}"
    );
    assert!(matches!(
        runtime.current()[0].lifecycle(),
        Lifecycle::Failed(message)
            if message == "MCP credential refresh failed. Run /mcp auth remote --open."
    ));
    assert_eq!(
        runtime.model_catalog()[0].availability,
        Availability::AuthenticationRequired
    );
    assert!(runtime.tools().is_empty());
    for secret in [
        "stored-token",
        "stored-refresh",
        "short-token",
        "next-refresh",
    ] {
        assert!(!health.contains(secret), "{health}");
        assert!(!output.content.contains(secret), "{}", output.content);
    }
}

#[tokio::test]
async fn a_rejected_notification_stream_fails_the_server_at_its_next_call() {
    use crate::test_support::{FakeServer, Reply};
    let server = FakeServer::start(|request| {
        if request.method == "GET" {
            return Reply::status(401).header("WWW-Authenticate", CHALLENGE);
        }
        echo_server_reply(request, r#"{"listChanged":true}"#, "")
    })
    .await;
    let runtime = remote_runtime(&server.url);
    runtime.connect(StartupPhase::All).await;
    assert!(server.wait_for(|request| request.method == "GET").await);
    let Lifecycle::Ready(client) = runtime.current()[0].lifecycle() else {
        panic!("the server connected");
    };
    for _ in 0..100 {
        if client.transport.stream_rejected_authentication() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(client.transport.stream_rejected_authentication());
    assert_eq!(runtime.model_catalog()[0].availability, Availability::Ready);
    let output = call(&runtime, "mcp_remote_echo", "{}").await;
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert!(
        output.content.contains("McpAuthenticationRequired"),
        "{}",
        output.content
    );
    needs_authentication(&runtime);
    assert!(
        !server
            .requests()
            .iter()
            .any(|request| request.method_name().as_deref() == Some("tools/call"))
    );
}

#[tokio::test]
async fn a_tool_list_refresh_answered_with_401_retires_the_server() {
    use std::sync::atomic::AtomicUsize;

    use crate::test_support::{FakeServer, Reply};
    let listings = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&listings);
    let server = FakeServer::start(move |request| {
        if request.method_name().as_deref() == Some("tools/list")
            && counted.fetch_add(1, Ordering::AcqRel) > 0
        {
            return Reply::status(401).header("WWW-Authenticate", CHALLENGE);
        }
        echo_server_reply(request, "{}", r#","ttlMs":1"#)
    })
    .await;
    let runtime = Arc::new(remote_runtime(&server.url));
    runtime.connect(StartupPhase::All).await;
    assert_eq!(runtime.model_catalog()[0].availability, Availability::Ready);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let searched = McpToolSearch::search_tools(Arc::clone(&runtime), request("remote tools")).await;
    assert_eq!(searched.model_output, GUIDANCE);
    assert_eq!(listings.load(Ordering::Acquire), 2);
    needs_authentication(&runtime);
    assert!(runtime.tools().is_empty());
}
