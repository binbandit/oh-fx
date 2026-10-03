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
            Some(ParseRefreshError::InvalidShape)
        );
    }
}

use ofx_testkit::{FakeServer, Reply};

fn authorization_value(prepared: &GrokSignIn, key: &str) -> zeroize::Zeroizing<String> {
    query_value_non_empty(prepared.authorization_url().split_once('?').unwrap().1, key).unwrap()
}

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
    assert!(saved.expires_at_ms > now_ms());
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
    let redirect = authorization_value(&prepared, "redirect_uri");
    let redirect_url = reqwest::Url::parse(&redirect).unwrap();
    let port = redirect_url.port().unwrap();
    assert!(![8976, 8977].contains(&port));
    assert_eq!(redirect_url.path(), "/callback");
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
    let body = String::from_utf8_lossy(&requests[0].body);
    let verifier = query_value_non_empty(&body, "code_verifier").unwrap();
    assert_eq!(
        oauth::pkce_challenge(&verifier),
        authorization_value(&prepared, "code_challenge").as_str()
    );
    let expected = format!(
        "grant_type=authorization_code&client_id={CLIENT_ID}&code=auth%2Bcode&code_verifier={}&redirect_uri={}",
        verifier.as_str(),
        {
            let mut form = FormBody::default();
            form.append("redirect_uri", &redirect);
            form.as_str()
                .strip_prefix("redirect_uri=")
                .unwrap()
                .to_owned()
        }
    );
    assert_eq!(requests[0].body, expected.as_bytes());
    assert_eq!(requests[0].path, "/v1/token");
    assert_eq!(
        requests[0].header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(requests[1].path, "/v1/userinfo");
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
    for redirect in [true, false] {
        let destination = FakeServer::start([]);
        let reply = if redirect {
            Reply::status_with_headers(302, &[("Location", &destination.base_url())], "")
        } else {
            Reply::status(200, "x".repeat(64 * 1024 + 1))
        };
        let server = FakeServer::start([reply]);
        let directory = tempfile::tempdir().unwrap();
        let oauth = oauth_for(&server, directory.path().join("profile"));
        assert_eq!(
            oauth.fetch_account_id("access").await.err(),
            Some(if redirect {
                GrokError::GrokUserInfoRequestFailed
            } else {
                GrokError::OAuthResponseTooLarge
            })
        );
        assert_eq!(server.requests().len(), 1);
        assert!(destination.requests().is_empty());
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
    let redirect = reqwest::Url::parse(&authorization_value(&prepared, "redirect_uri")).unwrap();
    let state = authorization_value(&prepared, "state");
    let callback = async {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
            .await
            .unwrap();
        socket.write_all(format!("GET /callback?code=browser-code&state={} HTTP/1.1\r\nOrigin: https://accounts.x.ai\r\n\r\n",state.as_str()).as_bytes()).await.unwrap();
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

#[tokio::test]
async fn grok_cancelled_signin_waiting_for_storage_preserves_previous_session() {
    for retain_previous in [false, true] {
        let server = FakeServer::start([
            Reply::status(
                200,
                r#"{"access_token":"new","refresh_token":"rotated","expires_in":3600}"#,
            ),
            Reply::status(200, r#"{"sub":"acct_test"}"#),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let oauth = oauth_for(&server, directory.path().join("profile"));
        let previous = stored_session();
        oauth.store.save_new_session(&previous).await.unwrap();
        let mutation = oauth
            .store
            .begin_existing_mutation()
            .await
            .unwrap()
            .unwrap();
        if !retain_previous {
            mutation.delete().unwrap();
        }
        let prepared = oauth.start_sign_in().await.unwrap();
        prepared.submit_manual_code("code").unwrap();
        let cancel = CancellationToken::new();
        let interrupt = async {
            while server.requests().len() < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel.cancel();
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(mutation);
        };
        let (result, ()) = tokio::join!(prepared.finish(&cancel), interrupt);
        assert_eq!(result, Err(GrokError::Cancelled));
        assert_eq!(
            oauth.store.load().unwrap(),
            retain_previous.then_some(previous)
        );
    }
}

#[tokio::test]
async fn grok_refresh_cancellation_bounds_token_and_userinfo_waits() {
    for after_token in [false, true] {
        let gate = ofx_testkit::Gate::default();
        let token = Reply::status(
            200,
            r#"{"access_token":"new","refresh_token":"rotated","expires_in":3600}"#,
        );
        let userinfo = Reply::status(200, r#"{"sub":"acct_test"}"#);
        let replies = if after_token {
            [token, userinfo.after(&gate)]
        } else {
            [token.after(&gate), userinfo]
        };
        let server = FakeServer::start(replies);
        let directory = tempfile::tempdir().unwrap();
        let oauth = oauth_for(&server, directory.path().join("profile"));
        let previous = stored_session();
        oauth.store.save_new_session(&previous).await.unwrap();
        let cancel = CancellationToken::new();
        let interrupt = async {
            while server.requests().len() < if after_token { 2 } else { 1 } {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            cancel.cancel();
        };
        let loaded = tokio::time::timeout(Duration::from_secs(3), async {
            let (result, ()) = tokio::join!(
                oauth.load_access(GrokRefreshMode::Force, &cancel),
                interrupt
            );
            result
        })
        .await;
        gate.open();
        assert!(
            loaded.is_ok(),
            "refresh ignored cancellation: after_token={after_token}"
        );
        assert_eq!(loaded.unwrap().err(), Some(GrokError::Cancelled));
        assert_eq!(
            oauth.store.load().unwrap(),
            if after_token { None } else { Some(previous) }
        );
    }
}

#[tokio::test]
async fn grok_refresh_rotation_arriving_during_cancellation_is_saved() {
    let gate = ofx_testkit::Gate::default();
    let server = FakeServer::start([
        Reply::status(
            200,
            r#"{"access_token":"new","refresh_token":"rotated","expires_in":3600}"#,
        )
        .after(&gate),
        Reply::status(200, r#"{"sub":"acct_test"}"#),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    oauth
        .store
        .save_new_session(&stored_session())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let interrupt = async {
        while server.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(30)).await;
        gate.open();
    };
    let (result, ()) = tokio::join!(
        oauth.load_access(GrokRefreshMode::Force, &cancel),
        interrupt
    );
    assert_eq!(result.unwrap().unwrap().access_token.expose(), "new");
    assert_eq!(
        oauth.store.load().unwrap().unwrap().refresh_token.expose(),
        "rotated"
    );
}

#[tokio::test]
async fn grok_refresh_cancellation_has_one_grace_across_both_requests() {
    let token_gate = ofx_testkit::Gate::default();
    let identity_gate = ofx_testkit::Gate::default();
    let server = FakeServer::start([
        Reply::status(
            200,
            r#"{"access_token":"new","refresh_token":"rotated","expires_in":3600}"#,
        )
        .after(&token_gate),
        Reply::status(200, r#"{"sub":"acct_test"}"#).after(&identity_gate),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    oauth
        .store
        .save_new_session(&stored_session())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let interrupt = async {
        while server.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        token_gate.open();
        while server.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
        identity_gate.open();
    };
    let (result, ()) = tokio::join!(
        oauth.load_access(GrokRefreshMode::Force, &cancel),
        interrupt
    );
    assert_eq!(result.err(), Some(GrokError::Cancelled));
    assert!(oauth.store.load().unwrap().is_none());
}

#[tokio::test]
async fn grok_signin_debug_redacts_browser_authorization_secrets() {
    let server = FakeServer::start([]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let prepared = oauth.start_sign_in().await.unwrap();
    let debug = format!("{prepared:?}");
    assert!(!debug.contains(prepared.authorization_url()));
    let state = query_value_non_empty(
        prepared.authorization_url().split_once('?').unwrap().1,
        "state",
    )
    .unwrap();
    assert!(!debug.contains(state.as_str()));
}

#[tokio::test]
async fn grok_refresh_non_json_success_response_preserves_previous_session() {
    let server = FakeServer::start([Reply::status(200, "<html>proxy</html>")]);
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth_for(&server, directory.path().join("profile"));
    let previous = stored_session();
    oauth.store.save_new_session(&previous).await.unwrap();
    assert_eq!(
        oauth
            .load_access(GrokRefreshMode::Force, &CancellationToken::new())
            .await
            .err(),
        Some(GrokError::GrokOAuthRequestFailed)
    );
    assert_eq!(oauth.store.load().unwrap(), Some(previous));
}

#[test]
fn grok_constructor_rejects_non_loopback_oauth_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let defaults = GrokEndpoints::default();
    for index in 0..4 {
        let mut endpoints = defaults.clone();
        let target = match index {
            0 => &mut endpoints.issuer,
            1 => &mut endpoints.token_url,
            2 => &mut endpoints.userinfo_url,
            _ => &mut endpoints.revoke_url,
        };
        *target = "https://example.com/oauth".to_owned();
        assert_eq!(
            GrokOAuth::new(directory.path().join("profile"), "oh-fx/test", endpoints).unwrap_err(),
            GrokError::InvalidGrokOAuthEndpoint
        );
    }
}
