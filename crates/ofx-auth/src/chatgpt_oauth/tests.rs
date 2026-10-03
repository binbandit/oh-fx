use std::time::Instant;

use ofx_testkit::{FakeServer, Reply};

use super::*;

fn token_with(claims: &Value) -> String {
    format!(
        "header.{}.signature",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

fn account_token(account: &str) -> String {
    token_with(&serde_json::json!({
        JWT_AUTH_CLAIM: {"chatgpt_account_id": account},
        "exp": 4_102_444_800_i64,
    }))
}

fn oauth_for(server: &FakeServer, directory: PathBuf) -> ChatGptOAuth {
    ChatGptOAuth::new(
        directory,
        "oh-fx/test",
        ChatGptEndpoints {
            issuer: server.base_url(),
            token_url: format!("{}/oauth/token", server.base_url()),
            callback_ports: vec![0],
        },
    )
    .unwrap()
}

async fn saved_session(oauth: &ChatGptOAuth) -> Option<Session> {
    let mutation = oauth.store.begin_existing_mutation().await.unwrap()?;
    mutation.load().unwrap()
}

fn stored_session(access_token: &str, expires_at_ms: i64) -> Session {
    Session {
        access_token: Secret::new(access_token.to_owned()),
        refresh_token: Secret::new("refresh-original".to_owned()),
        expires_at_ms,
        account_id: "acct_test".to_owned(),
    }
}

async fn expired_login(oauth: &ChatGptOAuth) {
    oauth
        .store
        .save_new_session(&stored_session(&account_token("acct_test"), 1))
        .await
        .unwrap();
}

async fn saved_refresh_token(oauth: &ChatGptOAuth) -> String {
    saved_session(oauth)
        .await
        .unwrap()
        .refresh_token
        .expose()
        .to_owned()
}

async fn requested(server: &FakeServer) -> Instant {
    let started = Instant::now();
    while server.requests().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the refresh never reached the token endpoint"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Instant::now()
}

#[test]
fn chatgpt_account_id_is_extracted_from_the_namespaced_jwt_claim() {
    let token =
        token_with(&serde_json::json!({JWT_AUTH_CLAIM: {"chatgpt_account_id": "acct_test"}}));
    assert_eq!(extract_account_id(&token).unwrap(), "acct_test");
    for invalid in [
        "header.payload",
        "a.b.c.d",
        "header.!!!.signature",
        &token_with(&serde_json::json!({"chatgpt_account_id": "acct_test"})),
        &token_with(&serde_json::json!({JWT_AUTH_CLAIM: {"chatgpt_account_id": ""}})),
    ] {
        assert_eq!(
            extract_account_id(invalid),
            Err(ChatGptError::InvalidChatGptAccessToken)
        );
    }
}

#[tokio::test]
async fn codex_refresh_uses_json_and_accepts_omitted_token_rotation_and_lifetime() {
    let server = FakeServer::start([Reply::status(
        200,
        r#"{"access_token":"header.payload.signature"}"#,
    )]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    let response = oauth
        .request_refresh_token(
            r#"{"client_id":"client","grant_type":"refresh_token","refresh_token":"refresh"}"#,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = server.requests();
    assert_eq!(requests[0].path, "/v1/oauth/token");
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
    assert!(
        requests[0]
            .body_text()
            .contains("\"grant_type\":\"refresh_token\"")
    );
    assert!(response.refresh_token.is_none());
    assert!(response.expires_in.is_none());
}

#[test]
fn chatgpt_callbacks_redirect_to_the_loopback_address_the_listener_binds() {
    assert_eq!(
        callback_redirect_uri(1455),
        "http://127.0.0.1:1455/auth/callback"
    );
    assert_eq!(
        callback_redirect_uri(1457),
        "http://127.0.0.1:1457/auth/callback"
    );
}

#[test]
fn sign_in_expiry_comes_from_expires_in_or_the_access_token_exp_claim() {
    let token = account_token("acct_test");
    assert_eq!(session_expiry_ms(Some(60), &token, 1_000), Ok(61_000));
    assert_eq!(
        session_expiry_ms(None, &token, 1_000),
        Ok(4_102_444_800_000)
    );
    assert_eq!(
        session_expiry_ms(None, "not-a-jwt", 1_000),
        Err(ChatGptError::InvalidChatGptAccessToken)
    );
    assert!(session_expiry_ms(Some(0), &token, 1_000).is_err());
}

#[test]
fn chatgpt_browser_authorization_url_uses_pkce_without_device_authentication() {
    let url = build_browser_authorization_url(
        "https://auth.openai.com",
        &callback_redirect_uri(1455),
        "challenge-value",
        "state-value",
    );
    assert_eq!(
        url,
        "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&scope=openid%20profile%20email%20offline_access%20api.connectors.read%20api.connectors.invoke&code_challenge=challenge-value&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state=state-value&originator=oh-fx"
    );
    assert!(!url.contains("device"));
    assert!(
        build_browser_authorization_url("https://issuer//", "r", "c", "s")
            .starts_with("https://issuer/oauth/authorize?")
    );
}

#[test]
fn chatgpt_browser_callback_classifier_keeps_stale_callbacks_unrelated() {
    for target in [
        "/auth/callback?code=stale&state=other",
        "/auth/callback?error=access_denied&state=other",
        "/auth/callback?error=access_denied",
        "/auth/callback?code=x&state=expected#fragment",
        "/auth/callback?code=&state=expected",
    ] {
        assert!(
            matches!(
                classify_browser_callback(target, "expected"),
                ParseResult::Unrelated
            ),
            "{target}"
        );
    }
}

#[test]
fn chatgpt_browser_callback_classifier_reports_a_current_denial() {
    assert!(matches!(
        classify_browser_callback(
            "/auth/callback?error=access_denied&state=expected",
            "expected"
        ),
        ParseResult::Failed(ChatGptError::ChatGptAuthorizationFailed)
    ));
}

#[test]
fn chatgpt_browser_callback_requires_the_exact_path_and_state() {
    let code =
        parse_browser_callback_target("/auth/callback?code=auth%20code&state=expected", "expected")
            .unwrap();
    assert_eq!(code.expose(), "auth code");
    assert_eq!(
        parse_browser_callback_target("/auth/callback?code=auth&state=other", "expected")
            .unwrap_err(),
        CallbackError::StateMismatch
    );
    assert_eq!(
        parse_browser_callback_target("/auth/callback?code=auth&state=expecte", "expected")
            .unwrap_err(),
        CallbackError::StateMismatch
    );
    assert_eq!(
        parse_browser_callback_target("/other?code=auth&state=expected", "expected").unwrap_err(),
        CallbackError::Invalid
    );
}

#[tokio::test]
async fn refresh_replaces_an_expired_session_and_persists_the_rotation() {
    let fresh = account_token("acct_test");
    let server = FakeServer::start([Reply::status(
        200,
        serde_json::json!({"access_token": fresh, "refresh_token": "refresh-rotated", "expires_in": 3600}).to_string(),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("oh-fx");
    let oauth = oauth_for(&server, data.clone());
    oauth
        .store
        .save_new_session(&stored_session(&account_token("acct_test"), 1))
        .await
        .unwrap();
    let access = oauth
        .load_access(RefreshMode::IfNeeded, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(access.access_token(), fresh);
    assert!(access.refresh_after_ms() > now_ms());
    let body = server.requests()[0].body_text();
    assert_eq!(
        body,
        r#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann","grant_type":"refresh_token","refresh_token":"refresh-original"}"#
    );
    let saved = saved_session(&oauth).await.unwrap();
    assert_eq!(saved.refresh_token.expose(), "refresh-rotated");
    assert_eq!(saved.access_token.expose(), fresh);
}

#[tokio::test]
async fn unexpired_sessions_are_returned_without_a_refresh() {
    let server = FakeServer::start([]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    let token = account_token("acct_test");
    oauth
        .store
        .save_new_session(&stored_session(&token, now_ms() + 3_600_000))
        .await
        .unwrap();
    let access = oauth
        .load_access(RefreshMode::IfNeeded, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(access.access_token(), token);
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn terminal_refresh_rejections_retire_the_session() {
    let server = FakeServer::start([Reply::status(
        400,
        r#"{"error":{"code":"refresh_token_expired","message":"expired"}}"#,
    )]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    oauth
        .store
        .save_new_session(&stored_session(&account_token("acct_test"), 1))
        .await
        .unwrap();
    assert_eq!(
        oauth
            .load_access(RefreshMode::IfNeeded, &CancellationToken::new())
            .await
            .unwrap_err(),
        ChatGptError::CredentialRefreshRejected
    );
    assert!(saved_session(&oauth).await.is_none());
}

#[tokio::test]
async fn transient_refresh_failures_keep_the_session() {
    let server = FakeServer::start([Reply::status(503, r#"{"error":"busy"}"#)]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    oauth
        .store
        .save_new_session(&stored_session(&account_token("acct_test"), 1))
        .await
        .unwrap();
    assert_eq!(
        oauth
            .load_access(RefreshMode::Force, &CancellationToken::new())
            .await
            .unwrap_err(),
        ChatGptError::ChatGptOAuthRequestFailed
    );
    assert!(saved_session(&oauth).await.is_some());
}

#[tokio::test]
async fn refreshed_tokens_for_another_account_retire_the_session() {
    let server = FakeServer::start([Reply::status(
        200,
        serde_json::json!({"access_token": account_token("acct_other"), "expires_in": 60})
            .to_string(),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    oauth
        .store
        .save_new_session(&stored_session(&account_token("acct_test"), 1))
        .await
        .unwrap();
    assert_eq!(
        oauth
            .load_access(RefreshMode::Force, &CancellationToken::new())
            .await
            .unwrap_err(),
        ChatGptError::ChatGptAccountChanged
    );
    assert!(saved_session(&oauth).await.is_none());
}

#[tokio::test]
async fn a_cancelled_load_never_starts_a_refresh() {
    let server = FakeServer::start([]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    expired_login(&oauth).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        oauth
            .load_access(RefreshMode::IfNeeded, &cancel)
            .await
            .unwrap_err(),
        ChatGptError::Cancelled
    );
    assert!(server.requests().is_empty());
    assert_eq!(saved_refresh_token(&oauth).await, "refresh-original");
}

#[tokio::test]
async fn a_cancelled_load_stops_waiting_for_a_held_lock() {
    let server = FakeServer::start([]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    expired_login(&oauth).await;
    let held = oauth
        .store
        .begin_existing_mutation()
        .await
        .unwrap()
        .unwrap();
    let cancel = CancellationToken::new();
    let interrupt = async {
        tokio::task::yield_now().await;
        cancel.cancel();
    };
    let (loaded, ()) = tokio::join!(oauth.load_access(RefreshMode::IfNeeded, &cancel), interrupt);
    assert_eq!(loaded.unwrap_err(), ChatGptError::Cancelled);
    drop(held);
    assert!(server.requests().is_empty());
    assert_eq!(saved_refresh_token(&oauth).await, "refresh-original");
}

#[tokio::test]
async fn a_cancelled_refresh_stops_waiting_for_a_stalled_token_endpoint() {
    let server = FakeServer::start([Reply::held_status_with_headers(200, &[], "")]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    expired_login(&oauth).await;
    let cancel = CancellationToken::new();
    let interrupt = async {
        let cancelled = requested(&server).await;
        cancel.cancel();
        cancelled
    };
    let (loaded, cancelled) =
        tokio::join!(oauth.load_access(RefreshMode::IfNeeded, &cancel), interrupt);
    assert_eq!(loaded.unwrap_err(), ChatGptError::Cancelled);
    let waited = cancelled.elapsed();
    assert!(waited >= REFRESH_CANCEL_GRACE, "{waited:?}");
    assert!(waited < Duration::from_secs(10), "{waited:?}");
    assert_eq!(saved_refresh_token(&oauth).await, "refresh-original");
}

#[tokio::test]
async fn a_refresh_answered_within_the_cancel_grace_is_saved() {
    let fresh = account_token("acct_test");
    let server = FakeServer::start([Reply::delayed_status(
        200,
        serde_json::json!({"access_token": fresh, "refresh_token": "refresh-rotated", "expires_in": 3600}).to_string(),
        REFRESH_CANCEL_GRACE / 2,
    )]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    expired_login(&oauth).await;
    let cancel = CancellationToken::new();
    let interrupt = async {
        requested(&server).await;
        cancel.cancel();
    };
    let load = async {
        let loaded = oauth.load_access(RefreshMode::IfNeeded, &cancel).await;
        (loaded, cancel.is_cancelled())
    };
    let ((loaded, cancelled_when_loaded), ()) = tokio::join!(load, interrupt);
    assert!(cancelled_when_loaded, "the reply arrived before the cancel");
    assert_eq!(loaded.unwrap().unwrap().access_token(), fresh);
    assert_eq!(saved_refresh_token(&oauth).await, "refresh-rotated");
}

#[test]
fn refreshes_without_a_lifetime_use_the_access_token_expiry() {
    let token = token_with(&serde_json::json!({
        JWT_AUTH_CLAIM: {"chatgpt_account_id": "acct_test"},
        "exp": 2_000,
    }));
    let replacement = refresh_replacement(
        RefreshTokenResponse {
            access_token: Secret::new(token),
            refresh_token: None,
            expires_in: None,
        },
        &stored_session("old", 1),
        5,
    )
    .unwrap();
    assert_eq!(replacement.expires_at_ms, 2_000_000);
    assert_eq!(replacement.refresh_token.expose(), "refresh-original");
    for invalid in [
        &br#"{"access_token":""}"#[..],
        br#"{"access_token":"a","refresh_token":""}"#,
        br#"{"access_token":"a","expires_in":0}"#,
        br#"{"access_token":"a","expires_in":"60"}"#,
    ] {
        assert_eq!(
            parse_refresh_token_response(invalid).unwrap_err(),
            ChatGptError::InvalidChatGptOAuthResponse
        );
    }
}

#[test]
fn terminal_refresh_codes_require_a_new_sign_in() {
    for body in [
        r#"{"error":"invalid_grant"}"#,
        r#"{"error":{"code":"refresh_token_reused"}}"#,
        r#"{"error":{"code":"refresh_token_invalidated"}}"#,
    ] {
        assert!(chatgpt_refresh_requires_sign_in(body.as_bytes()), "{body}");
    }
    assert!(!chatgpt_refresh_requires_sign_in(
        br#"{"error":"server_error"}"#
    ));
}

#[tokio::test]
async fn non_json_successful_refresh_keeps_the_session() {
    let server = FakeServer::start([Reply::status(200, "upstream unavailable")]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("oh-fx"));
    oauth
        .store
        .save_new_session(&stored_session(&account_token("acct_test"), 1))
        .await
        .unwrap();
    assert_eq!(
        oauth
            .load_access(RefreshMode::Force, &CancellationToken::new())
            .await
            .unwrap_err(),
        ChatGptError::ChatGptOAuthRequestFailed
    );
    assert!(saved_session(&oauth).await.is_some());
}
