use super::*;

#[test]
fn grok_callback_rejects_wrong_state_and_reports_current_denial() {
    assert!(matches!(
        classify_browser_callback("/callback?code=stale&state=other", "expected"),
        ParseResult::Failed(GrokError::GrokOAuthStateMismatch)
    ));
    assert!(matches!(
        classify_browser_callback("/callback?error=access_denied&state=other", "expected"),
        ParseResult::Failed(GrokError::GrokOAuthStateMismatch)
    ));
    assert!(matches!(
        classify_browser_callback("/callback?error=access_denied&state=expected", "expected"),
        ParseResult::Failed(GrokError::GrokAuthorizationFailed)
    ));
    assert!(matches!(
        classify_browser_callback("/other?code=auth&state=expected", "expected"),
        ParseResult::Unrelated
    ));
    assert_eq!(
        parse_browser_callback_target("/callback?code=auth%20code&state=expected", "expected")
            .unwrap()
            .expose(),
        "auth code"
    );
}

#[test]
fn grok_authorization_uses_pkce_and_required_scope_in_exact_order() {
    assert_eq!(
        build_browser_authorization_url(
            "https://auth.x.ai/",
            "http://127.0.0.1:1234/callback",
            "challenge",
            "state"
        ),
        "https://auth.x.ai/oauth2/authorize?response_type=code&client_id=b1a00492-073a-47ea-816f-4c329264a828&redirect_uri=http%3A%2F%2F127.0.0.1%3A1234%2Fcallback&scope=openid%20profile%20email%20offline_access%20grok-cli%3Aaccess%20api%3Aaccess&code_challenge=challenge&code_challenge_method=S256&state=state&referrer=fx"
    );
}

#[test]
fn grok_refresh_accepts_missing_rotation_but_rejects_malformed_lifetimes() {
    let parsed =
        parse_refresh_token_response(br#"{"access_token":"new","expires_in":3600}"#).unwrap();
    assert!(parsed.refresh_token.is_none());
    assert_eq!(parsed.expires_in, Some(3600));
    for body in [
        br#"{"access_token":"new","refresh_token":""}"#.as_slice(),
        br#"{"access_token":"new","expires_in":0}"#,
        br#"{"access_token":"new","expires_in":1.0}"#,
    ] {
        assert_eq!(
            parse_refresh_token_response(body).err(),
            Some(GrokError::InvalidGrokOAuthResponse)
        );
    }
}

use ofx_testkit::{FakeServer, Reply};

fn oauth_for(server: &FakeServer, directory: PathBuf) -> GrokOAuth {
    GrokOAuth::new(
        directory,
        "oh-fx/test",
        GrokEndpoints {
            issuer: server.base_url(),
            token_url: format!("{}/token", server.base_url()),
            userinfo_url: format!("{}/userinfo", server.base_url()),
            revoke_url: format!("{}/revoke", server.base_url()),
        },
    )
    .unwrap()
}

fn stored_session() -> Session {
    Session {
        access_token: Secret::new("old-access".to_owned()),
        refresh_token: Secret::new("refresh+original".to_owned()),
        expires_at_ms: 1,
        account_id: "acct_test".to_owned(),
    }
}

#[tokio::test]
async fn grok_refresh_uses_form_and_authenticated_userinfo_with_optional_rotation() {
    let server = FakeServer::start([
        Reply::status(200, r#"{"access_token":"new-access","expires_in":3600}"#),
        Reply::status(200, r#"{"sub":"acct_test"}"#),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    oauth
        .store
        .save_new_session(&stored_session())
        .await
        .unwrap();
    let access = oauth
        .load_access(GrokRefreshMode::IfNeeded, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(access.account_id(), "acct_test");
    assert_eq!(access.into_token(), "new-access");
    let saved = oauth.store.load().unwrap().unwrap();
    assert_eq!(saved.refresh_token.expose(), "refresh+original");
    assert!(saved.expires_at_ms > crate::chatgpt_oauth::now_ms());
    let requests = server.requests();
    assert_eq!(requests[0].body,b"grant_type=refresh_token&client_id=b1a00492-073a-47ea-816f-4c329264a828&refresh_token=refresh%2Boriginal");
    assert_eq!(requests[1].method, "GET");
    assert!(
        requests[1]
            .headers
            .iter()
            .any(|(key, value)| key.eq_ignore_ascii_case("authorization")
                && value == "Bearer new-access")
    );
}

#[tokio::test]
async fn grok_refresh_retires_terminal_tokens_and_changed_or_unsafe_identity() {
    for (replies, expected) in [
        (
            vec![Reply::status(400, r#"{"error":"invalid_grant"}"#)],
            GrokError::CredentialRefreshRejected,
        ),
        (
            vec![Reply::status(
                200,
                r#"{"access_token":"new","expires_in":0}"#,
            )],
            GrokError::CredentialRefreshRejected,
        ),
        (
            vec![
                Reply::status(200, r#"{"access_token":"new"}"#),
                Reply::status(200, r#"{"sub":"acct_test"}"#),
            ],
            GrokError::CredentialRefreshRejected,
        ),
        (
            vec![
                Reply::status(200, r#"{"access_token":"new","expires_in":3600}"#),
                Reply::status(200, r#"{"sub":"different"}"#),
            ],
            GrokError::GrokAccountChanged,
        ),
        (
            vec![
                Reply::status(200, r#"{"access_token":"new","expires_in":3600}"#),
                Reply::status(200, r#"{"sub":"acct\ninjected"}"#),
            ],
            GrokError::CredentialRefreshRejected,
        ),
    ] {
        let server = FakeServer::start(replies);
        let directory = tempfile::tempdir().unwrap();
        let oauth = oauth_for(&server, directory.path().join("profile"));
        oauth
            .store
            .save_new_session(&stored_session())
            .await
            .unwrap();
        assert_eq!(
            oauth
                .load_access(GrokRefreshMode::IfNeeded, &CancellationToken::new())
                .await
                .err(),
            Some(expected)
        );
        assert!(oauth.store.load().unwrap().is_none());
    }
}

#[tokio::test]
async fn grok_refresh_preserves_session_on_transient_http_failure() {
    let server = FakeServer::start([Reply::status(503, "unavailable")]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    oauth
        .store
        .save_new_session(&stored_session())
        .await
        .unwrap();
    assert_eq!(
        oauth
            .load_access(GrokRefreshMode::IfNeeded, &CancellationToken::new())
            .await
            .err(),
        Some(GrokError::GrokOAuthRequestFailed)
    );
    assert_eq!(oauth.store.load().unwrap(), Some(stored_session()));
}

#[tokio::test]
async fn grok_refresh_is_singleflight_and_stored_mode_never_refreshes() {
    let server = FakeServer::start([
        Reply::status(
            200,
            r#"{"access_token":"new","refresh_token":"rotated","expires_in":3600}"#,
        ),
        Reply::status(200, r#"{"sub":"acct_test"}"#),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    oauth
        .store
        .save_new_session(&stored_session())
        .await
        .unwrap();
    assert_eq!(
        oauth
            .load_access(GrokRefreshMode::Stored, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap()
            .into_token(),
        "old-access"
    );
    assert!(server.requests().is_empty());
    let cancel = CancellationToken::new();
    let (first, second) = tokio::join!(
        oauth.load_access(GrokRefreshMode::IfNeeded, &cancel),
        oauth.load_access(GrokRefreshMode::IfNeeded, &cancel)
    );
    assert_eq!(first.unwrap().unwrap().into_token(), "new");
    assert_eq!(second.unwrap().unwrap().into_token(), "new");
    assert_eq!(server.requests().len(), 2);
    assert_eq!(
        oauth.store.load().unwrap().unwrap().refresh_token.expose(),
        "rotated"
    );
}

#[tokio::test]
async fn grok_logout_revokes_refresh_token_then_deletes_even_on_rejection() {
    for status in [200, 503] {
        let server = FakeServer::start([Reply::status(status, "")]);
        let directory = tempfile::tempdir().unwrap();
        let oauth = oauth_for(&server, directory.path().join("profile"));
        oauth
            .store
            .save_new_session(&stored_session())
            .await
            .unwrap();
        assert_eq!(
            oauth.logout().await.unwrap(),
            GrokLogoutResult {
                deletion: DeleteOutcome::Deleted,
                revocation_failed: status != 200
            }
        );
        assert!(oauth.store.load().unwrap().is_none());
        assert_eq!(
            server.requests()[0].body,
            b"token=refresh%2Boriginal&client_id=b1a00492-073a-47ea-816f-4c329264a828"
        );
        assert_eq!(
            oauth.logout().await.unwrap(),
            GrokLogoutResult {
                deletion: DeleteOutcome::Missing,
                revocation_failed: false
            }
        );
        assert_eq!(server.requests().len(), 1);
    }
}

#[tokio::test]
async fn grok_manual_signin_uses_ephemeral_redirect_and_saves_userinfo_identity() {
    let server = FakeServer::start([
        Reply::status(
            200,
            r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
        ),
        Reply::status(200, r#"{"sub":"acct_test"}"#),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let prepared = oauth.start_sign_in().await.unwrap();
    assert_ne!(prepared.listener.port(), 0);
    assert!(prepared.redirect_uri.ends_with("/callback"));
    assert!(
        prepared
            .authorization_url()
            .contains("code_challenge_method=S256")
    );
    for (code, error) in [
        (" \t\r\n", GrokError::InvalidGrokAuthorizationCode),
        ("has space", GrokError::InvalidGrokAuthorizationCode),
        ("\u{b}code", GrokError::InvalidGrokAuthorizationCode),
        (&"x".repeat(4097), GrokError::InvalidGrokAuthorizationCode),
    ] {
        assert_eq!(prepared.submit_manual_code(code), Err(error));
    }
    prepared.submit_manual_code(" \tauth+code\r\n").unwrap();
    prepared.finish(&CancellationToken::new()).await.unwrap();
    assert_eq!(oauth.store.load().unwrap().unwrap().account_id, "acct_test");
    let requests = server.requests();
    let expected = format!(
        "grant_type=authorization_code&client_id={CLIENT_ID}&code=auth%2Bcode&code_verifier={}&redirect_uri={}",
        prepared.verifier.expose(),
        {
            let mut form = FormBody::default();
            form.append("redirect_uri", &prepared.redirect_uri);
            form.as_str()
                .strip_prefix("redirect_uri=")
                .unwrap()
                .to_owned()
        }
    );
    assert_eq!(requests[0].body, expected.as_bytes());
}

#[tokio::test]
async fn grok_cancelled_signin_publishes_no_session_or_request() {
    let server = FakeServer::start([]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let prepared = oauth.start_sign_in().await.unwrap();
    prepared.submit_manual_code("code").unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(prepared.finish(&cancel).await, Err(GrokError::Cancelled));
    assert!(server.requests().is_empty());
    assert!(oauth.store.load().unwrap().is_none());
}

#[test]
fn grok_endpoint_overrides_allow_only_explicit_loopback_http_ports() {
    for value in [
        "http://127.0.0.1:1234/token",
        "http://localhost:1234/token",
        "http://[::1]:1234/token",
        "http://localhost:80/token",
    ] {
        assert!(is_loopback_http_url(value), "{value}");
    }
    for value in [
        "https://127.0.0.1:1234/token",
        "http://example.com:1234/token",
        "http://localhost/token",
        "http://user@localhost:1234/token",
        "http://@localhost:1234/token",
    ] {
        assert!(!is_loopback_http_url(value), "{value}");
    }
}

#[tokio::test]
async fn grok_signin_is_one_shot_and_refuses_post_completion_manual_codes() {
    let server = FakeServer::start([
        Reply::status(
            200,
            r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
        ),
        Reply::status(200, r#"{"sub":"acct_test"}"#),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let prepared = oauth.start_sign_in().await.unwrap();
    prepared.submit_manual_code("code").unwrap();
    let cancel = CancellationToken::new();
    let (first, second) = tokio::join!(prepared.finish(&cancel), prepared.finish(&cancel));
    assert_eq!(first, Ok(()));
    assert_eq!(second, Err(GrokError::GrokLoginBusy));
    assert_eq!(
        prepared.finish(&cancel).await,
        Err(GrokError::GrokLoginBusy)
    );
    assert_eq!(
        prepared.submit_manual_code("another"),
        Err(GrokError::GrokLoginBusy)
    );
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn grok_signin_deadline_starts_when_the_flow_is_prepared() {
    let server = FakeServer::start([]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let prepared = oauth.start_sign_in().await.unwrap();
    prepared.submit_manual_code("code").unwrap();
    tokio::time::advance(LOGIN_TIMEOUT + Duration::from_secs(1)).await;
    assert_eq!(
        prepared.finish(&CancellationToken::new()).await,
        Err(GrokError::LoginTimedOut)
    );
    assert!(server.requests().is_empty());
    assert!(oauth.store.load().unwrap().is_none());
}

#[tokio::test]
async fn grok_manual_code_arriving_during_callback_wait_completes_signin() {
    let server = FakeServer::start([
        Reply::status(
            200,
            r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
        ),
        Reply::status(200, r#"{"sub":"acct_test"}"#),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let prepared = oauth.start_sign_in().await.unwrap();
    let cancel = CancellationToken::new();
    let submit = async {
        tokio::task::yield_now().await;
        prepared.submit_manual_code("code").unwrap();
    };
    let (result, ()) = tokio::join!(prepared.finish(&cancel), submit);
    assert_eq!(result, Ok(()));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn grok_userinfo_transport_rejects_redirects_and_bounds_response_size() {
    for reply in [
        Reply::status_with_headers(302, &[("Location", "http://example.com/")], ""),
        Reply::status(200, "x".repeat(64 * 1024 + 1)),
    ] {
        let server = FakeServer::start([reply]);
        let directory = tempfile::tempdir().unwrap();
        let oauth = oauth_for(&server, directory.path().join("profile"));
        assert!(matches!(
            oauth.fetch_account_id("access").await,
            Err(GrokError::GrokUserInfoRequestFailed | GrokError::OAuthResponseTooLarge)
        ));
        assert_eq!(server.requests().len(), 1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn grok_rotated_refresh_save_failure_retires_the_consumed_session() {
    use std::os::unix::fs::PermissionsExt;
    let gate = ofx_testkit::Gate::default();
    let server = FakeServer::start([
        Reply::status(
            200,
            r#"{"access_token":"new","refresh_token":"rotated","expires_in":3600}"#,
        ),
        Reply::status(200, r#"{"sub":"acct_test"}"#).after(&gate),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let oauth = oauth_for(&server, profile.clone());
    oauth
        .store
        .save_new_session(&stored_session())
        .await
        .unwrap();
    let sabotage = async {
        while server.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        std::fs::set_permissions(
            profile.join(crate::grok_session::AUTH_FILE_NAME),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        gate.open();
    };
    let cancel = CancellationToken::new();
    let (loaded, ()) = tokio::join!(oauth.load_access(GrokRefreshMode::Force, &cancel), sabotage);
    assert_eq!(
        loaded.err(),
        Some(GrokError::CredentialRefreshPersistenceUncertain)
    );
    assert!(oauth.store.load().unwrap().is_none());
}

#[tokio::test]
async fn grok_cancelled_inflight_signin_never_persists_returned_tokens() {
    let gate = ofx_testkit::Gate::default();
    let server = FakeServer::start([Reply::status(
        200,
        r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
    )
    .after(&gate)]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let prepared = oauth.start_sign_in().await.unwrap();
    prepared.submit_manual_code("code").unwrap();
    let cancel = CancellationToken::new();
    let interrupt = async {
        while server.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        cancel.cancel();
        gate.open();
    };
    let (result, ()) = tokio::join!(prepared.finish(&cancel), interrupt);
    assert_eq!(result, Err(GrokError::Cancelled));
    assert!(oauth.store.load().unwrap().is_none());
}

#[tokio::test]
async fn grok_callback_success_response_follows_authenticated_session_persistence() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = FakeServer::start([
        Reply::status(
            200,
            r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
        ),
        Reply::status(200, r#"{"sub":"acct_test"}"#),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let prepared = oauth.start_sign_in().await.unwrap();
    let callback = async {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", prepared.listener.port()))
            .await
            .unwrap();
        socket.write_all(format!("GET /callback?code=browser-code&state={} HTTP/1.1\r\nOrigin: https://accounts.x.ai\r\n\r\n",prepared.state.expose()).as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        assert!(oauth.store.load().unwrap().is_some());
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Access-Control-Allow-Origin: https://accounts.x.ai\r\n"));
    };
    let cancel = CancellationToken::new();
    let (result, ()) = tokio::join!(prepared.finish(&cancel), callback);
    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn grok_signin_rejects_missing_lifetime_before_requesting_userinfo() {
    let server = FakeServer::start([Reply::status(
        200,
        r#"{"access_token":"access","refresh_token":"refresh"}"#,
    )]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let prepared = oauth.start_sign_in().await.unwrap();
    prepared.submit_manual_code("code").unwrap();
    assert_eq!(
        prepared.finish(&CancellationToken::new()).await,
        Err(GrokError::InvalidGrokOAuthResponse)
    );
    assert_eq!(server.requests().len(), 1);
    assert!(oauth.store.load().unwrap().is_none());
}
