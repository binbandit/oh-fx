use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use ofx_auth::VercelEndpoints;
use ofx_contract::{
    ChatMessage, ModelProvider, ModelRequest, ProviderErrorKind, ProviderOptions, StreamEvent,
    ToolChoice,
};
use ofx_gateway::{GatewayEndpoints, GatewayProvider};
use ofx_testkit::{FakeServer, Gate, Reply};

use super::*;

const ROTATED: &str = "\"refresh_token\":\"rotated-refresh\"";

fn write_session(directory: &std::path::Path, issuer: &str, expires_at_ms: i64) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = directory.join("auth.json");
    std::fs::write(
        &file,
        format!(
            "{{\"version\":1,\"issuer\":\"{issuer}\",\"client_id\":\"client\",\"access_token\":\"login-access\",\"refresh_token\":\"login-refresh\",\"expires_at_ms\":{expires_at_ms},\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\"}}"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn issuer_with_held_token_reply(gate: &Gate) -> FakeServer {
    FakeServer::start_at_origin(|origin| {
        [
            Reply::status(
                200,
                format!(
                    r#"{{"issuer":"{origin}","device_authorization_endpoint":"{origin}/oauth/device","token_endpoint":"{origin}/oauth/token"}}"#
                ),
            ),
            Reply::status(
                200,
                r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
            )
            .after(gate),
        ]
    })
}

async fn due_login(
    data: &std::path::Path,
    server: &FakeServer,
    detached: Option<Arc<DetachedRefreshes>>,
) -> GatewayProvider {
    let issuer = format!("http://{}", server.address());
    write_session(data, &issuer, i64::MAX);
    let login = VercelOAuth::new(data.to_owned(), "oh-fx/test", VercelEndpoints::default());
    let mut key = login
        .load_credential(&CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    key.refresh_after_ms = Some(0);
    write_session(data, &issuer, 1);
    GatewayProvider::new(
        gateway_credential(&key, Some(login), detached),
        "oh-fx/test",
        GatewayEndpoints {
            chat: format!("http://{}/v4/ai/language-model", server.address()),
            ..GatewayEndpoints::default()
        },
    )
    .unwrap()
}

fn saved(data: &std::path::Path) -> String {
    std::fs::read_to_string(data.join("auth.json")).unwrap_or_default()
}

async fn cancel_once_the_token_is_requested(server: &FakeServer, cancel: &CancellationToken) {
    while server.requests().len() < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cancel.cancel();
}

async fn cancelled_turn(
    gateway: &GatewayProvider,
    cancel: &CancellationToken,
) -> ProviderErrorKind {
    let messages = [ChatMessage::user("question")];
    let request = ModelRequest {
        model: "openai/gpt-5.6-sol",
        instructions: &[],
        messages: &messages,
        tools: &[],
        tool_choice: ToolChoice::Auto,
        max_output_tokens: None,
        provider_options: ProviderOptions::default(),
        session_id: None,
    };
    let mut sink = |_: StreamEvent| {};
    gateway
        .stream(&request, &mut sink, cancel)
        .await
        .unwrap_err()
        .kind
}

#[test]
fn a_cancelled_turn_saves_a_sent_refresh_before_its_runtime_shuts_down() {
    let gate = Gate::default();
    let server = issuer_with_held_token_reply(&gate);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let kind = runtime.block_on(async {
        let gateway = due_login(&data, &server, None).await;
        let cancel = CancellationToken::new();
        let releasing = async {
            cancel_once_the_token_is_requested(&server, &cancel).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            gate.open();
        };
        let (kind, ()) = tokio::join!(cancelled_turn(&gateway, &cancel), releasing);
        kind
    });
    drop(runtime);
    assert_eq!(kind, ProviderErrorKind::Cancelled);
    assert!(saved(&data).contains(ROTATED), "{}", saved(&data));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn an_interactive_turn_stops_at_once_and_the_session_end_waits_for_the_save() {
    let gate = Gate::default();
    let server = issuer_with_held_token_reply(&gate);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    let detached = Arc::new(DetachedRefreshes::default());
    let gateway = due_login(&data, &server, Some(Arc::clone(&detached))).await;
    let cancel = CancellationToken::new();
    let (kind, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            cancelled_turn(&gateway, &cancel),
            cancel_once_the_token_is_requested(&server, &cancel)
        )
    })
    .await
    .expect("the cancelled turn ends while the token reply is held");
    assert_eq!(kind, ProviderErrorKind::Cancelled);
    assert!(detached.pending());
    assert!(!saved(&data).contains(ROTATED));
    gate.open();
    detached.close();
    tokio::time::timeout(Duration::from_secs(5), detached.settle())
        .await
        .expect("the session end waits for the refresh to save");
    assert!(saved(&data).contains(ROTATED), "{}", saved(&data));
}

fn slow_issuer(discovery: Duration, token: Duration) -> FakeServer {
    FakeServer::start_at_origin(|origin| {
        [
            Reply::delayed_status(
                200,
                format!(
                    r#"{{"issuer":"{origin}","device_authorization_endpoint":"{origin}/oauth/device","token_endpoint":"{origin}/oauth/token"}}"#
                ),
                discovery,
            ),
            Reply::delayed_status(
                200,
                r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
                token,
            ),
        ]
    })
}

fn refreshing_in_background(
    data: &std::path::Path,
    server: &FakeServer,
    detached: &Arc<DetachedRefreshes>,
) -> std::thread::JoinHandle<()> {
    write_session(data, &format!("http://{}", server.address()), 1);
    let refresh = LoginRefresh {
        login: VercelOAuth::new(data.to_owned(), "oh-fx/test", VercelEndpoints::default()),
        detached: Some(Arc::clone(detached)),
    };
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = runtime.block_on(refresh.refresh(false, &CancellationToken::new()));
    })
}

fn wait_for_requests(server: &FakeServer, count: usize) {
    while server.requests().len() < count {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_session_end_waits_for_a_sent_refresh_through_a_slow_issuer() {
    let server = slow_issuer(Duration::from_secs(1), Duration::from_millis(1_400));
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    let detached = Arc::new(DetachedRefreshes::with_limits(
        Duration::from_secs(2),
        Duration::from_millis(1_800),
    ));
    let refreshing = refreshing_in_background(&data, &server, &detached);
    wait_for_requests(&server, 2);
    detached.close();
    detached.wait_for_running();
    assert!(saved(&data).contains(ROTATED), "{}", saved(&data));
    refreshing.join().unwrap();
}

#[test]
fn the_session_end_stops_a_refresh_that_has_not_sent_its_token_request() {
    let server = slow_issuer(Duration::from_secs(1), Duration::ZERO);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    let detached = Arc::new(DetachedRefreshes::default());
    let refreshing = refreshing_in_background(&data, &server, &detached);
    wait_for_requests(&server, 1);
    let ending = std::time::Instant::now();
    detached.close();
    detached.wait_for_running();
    assert!(ending.elapsed() < Duration::from_millis(900));
    refreshing.join().unwrap();
    std::thread::sleep(Duration::from_millis(1_200));
    assert_eq!(server.requests().len(), 1);
    assert!(saved(&data).contains("\"refresh_token\":\"login-refresh\""));
}
