use std::sync::{Arc, Mutex};

use ofx_http::ConnectionOptions;
use reqwest::Response;
use tokio::sync::mpsc;

use super::HttpAuth;
use crate::auth_state::AuthState;
use crate::error::McpError;
use crate::health::AuthenticationState;
use crate::mcp_auth::Challenge;
use crate::mcp_contract::{McpServerConfig, TransportType};
use crate::server_connection::McpClient;
use crate::server_transport::{ConnectOptions, startup_deadline, startup_failure_message};
use crate::test_support::{FakeServer, RecordedRequest, Reply};

const CHALLENGE: &str =
    r#"Bearer scope="tools.call", resource_metadata="https://auth.example/prm""#;
const REQUIRED: &str =
    "Authentication required. Run /mcp auth remote --open, or configure bearer_token_env.";

fn challenged() -> Reply {
    Reply::status(401).header("WWW-Authenticate", CHALLENGE)
}

async fn get(http: &reqwest::Client, url: &str) -> Response {
    http.get(url).send().await.unwrap()
}

#[tokio::test]
async fn a_rejection_remembers_only_the_latest_challenge() {
    let server = FakeServer::start(|request| match request.path.as_str() {
        "/challenged" => challenged(),
        "/bare" => Reply::status(401),
        "/forbidden" => Reply::status(403),
        "/scoped" => Reply::status(403).header(
            "WWW-Authenticate",
            r#"Bearer error="insufficient_scope", scope="tools.admin""#,
        ),
        "/moved" => Reply::status(302).header("Location", "/elsewhere"),
        _ => Reply::status(200),
    })
    .await;
    let state = Arc::new(AuthState::default());
    let config = McpServerConfig::remote("remote", TransportType::Http, &server.url);
    let auth = HttpAuth::resolve(
        &config,
        None,
        &state,
        || Err(McpError::HttpClientUnavailable),
        &|_| None,
    )
    .await
    .ok()
    .unwrap();
    let http = ofx_http::build_connection_client(&ConnectionOptions {
        follow_redirects: false,
        ..ConnectionOptions::default()
    })
    .unwrap();
    let origin = server.url.trim_end_matches("/mcp").to_owned();
    let at = |path: &str| format!("{origin}{path}");
    assert_eq!(
        auth.reject(&get(&http, &at("/challenged")).await),
        Err(McpError::McpAuthenticationRequired)
    );
    assert_eq!(
        auth.reject(&get(&http, &at("/bare")).await),
        Err(McpError::McpAuthenticationRequired)
    );
    assert_eq!(auth.capture(), REQUIRED);
    assert_eq!(state.pending(), Challenge::default());
    assert_eq!(auth.reject(&get(&http, &at("/forbidden")).await), Ok(()));
    assert_eq!(
        auth.reject(&get(&http, &at("/scoped")).await),
        Err(McpError::McpAuthenticationRequired)
    );
    auth.capture();
    let pending = state.pending();
    assert!(pending.insufficient_scope);
    assert_eq!(pending.scope.as_deref(), Some("tools.admin"));
    assert_eq!(
        auth.reject(&get(&http, &at("/moved")).await),
        Err(McpError::RedirectNotAllowed)
    );
    assert_eq!(auth.reject(&get(&http, &at("/ok")).await), Ok(()));
}

fn http_reply(request: &RecordedRequest, rejected: &str) -> Reply {
    if request.method_name().as_deref() == Some(rejected) {
        return challenged();
    }
    let Some(id) = request.request_id() else {
        return Reply::status(202);
    };
    match request.method_name().as_deref() {
        Some("initialize") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"remote","version":"1"}}}}}}"#
        )),
        Some("tools/list") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[]}}}}"#
        )),
        _ => Reply::status(202),
    }
}

async fn sse_server(rejected: &'static str) -> FakeServer {
    let (events, live) = mpsc::unbounded_channel();
    let live = Mutex::new(Some(live));
    let events = Mutex::new(events);
    FakeServer::start(move |request| {
        if request.method == "GET" {
            if rejected == "discovery" {
                return challenged();
            }
            let sender = events.lock().unwrap().clone();
            let _ = sender.send("event: endpoint\ndata: /messages\n\n".to_owned());
            return live
                .lock()
                .unwrap()
                .take()
                .map_or_else(|| Reply::status(500), Reply::live_events);
        }
        if request.method_name().as_deref() == Some(rejected) {
            return challenged();
        }
        let answer = match (request.request_id(), request.method_name().as_deref()) {
            (Some(id), Some("initialize")) => Some(format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"remote"}}}}}}"#
            )),
            (Some(id), Some("tools/list")) => Some(format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[]}}}}"#
            )),
            _ => None,
        };
        if let Some(body) = answer {
            let _ = events
                .lock()
                .unwrap()
                .send(format!("event: message\ndata: {body}\n\n"));
        }
        Reply::status(202)
    })
    .await
}

async fn assert_startup_challenge(config: &McpServerConfig) {
    let state = Arc::new(AuthState::default());
    let failure = McpClient::connect_until(
        config,
        &ConnectOptions::default(),
        startup_deadline(config),
        &state,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(failure.error, McpError::McpAuthenticationRequired);
    assert_eq!(startup_failure_message(&failure, 5_000, 5_000), REQUIRED);
    assert_eq!(state.pending().scope.as_deref(), Some("tools.call"));
    assert_eq!(state.authentication(config), AuthenticationState::Required);
}

fn remote(transport: TransportType, url: &str) -> McpServerConfig {
    McpServerConfig {
        startup_timeout_ms: 5_000,
        ..McpServerConfig::remote("remote", transport, url)
    }
}

#[tokio::test]
async fn a_challenge_during_startup_fails_either_transport_with_upstreams_message() {
    for rejected in ["initialize", "tools/list"] {
        let server = FakeServer::start(move |request| http_reply(request, rejected)).await;
        assert_startup_challenge(&remote(TransportType::Http, &server.url)).await;
    }
    for rejected in ["discovery", "initialize", "tools/list"] {
        let server = sse_server(rejected).await;
        assert_startup_challenge(&remote(TransportType::Sse, &server.url)).await;
    }
}
