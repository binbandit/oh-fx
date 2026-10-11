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
