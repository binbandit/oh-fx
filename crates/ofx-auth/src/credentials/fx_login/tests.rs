use std::os::unix::fs::PermissionsExt;

use ofx_testkit::{FakeServer, Gate, Reply};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::credentials::tests::{login, write_session};

const TEAM: &str = ",\"team_slug\":\"team-slug\",\"team_id\":\"team_id\"";

fn metadata(origin: &str) -> Reply {
    Reply::status(
        200,
        format!(
            r#"{{"issuer":"{origin}","device_authorization_endpoint":"{origin}/oauth/device","token_endpoint":"{origin}/oauth/token"}}"#
        ),
    )
}

fn refreshed(token: Reply) -> FakeServer {
    FakeServer::start_at_origin(|origin| [metadata(origin), token])
}

fn origin(server: &FakeServer) -> String {
    format!("http://{}", server.address())
}

#[test]
fn credential_refresh_deadlines_keep_a_fresh_short_lived_token_usable() {
    assert_eq!(refresh_after_ms(130_000, None), 70_000);
    assert_eq!(refresh_after_ms(130_000, Some(100_000)), 130_000);
    assert_eq!(refresh_after_ms(200_000, Some(100_000)), 140_000);
}

#[tokio::test]
async fn a_fresh_login_is_used_without_any_request() {
    let server = FakeServer::start([]);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &origin(&server), i64::MAX, TEAM);
    let key = login(&data)
        .load_credential(&CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key.secret.as_str(), "login-access");
    assert_eq!(key.refresh_after_ms, Some(i64::MAX - 60_000));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn an_expired_login_is_refreshed_and_saved_with_its_team() {
    let server = refreshed(Reply::status(
        200,
        r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
    ));
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &origin(&server), 1, TEAM);
    let before = now_ms();
    let key = login(&data)
        .load_credential(&CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key.secret.as_str(), "new-access");
    assert_eq!(key.gateway_team(), Some("team_id"));
    assert!(key.refresh_after_ms.unwrap() >= before + 3_540_000);
    let requests = server.requests();
    assert_eq!(requests[1].path, "/oauth/token");
    assert_eq!(
        requests[1].body_text(),
        "client_id=client&grant_type=refresh_token&refresh_token=login-refresh"
    );
    let saved = std::fs::read_to_string(data.join("auth.json")).unwrap();
    assert!(
        saved.contains("\"access_token\":\"new-access\",\"refresh_token\":\"rotated-refresh\"")
    );
    assert!(saved.ends_with(",\"team_slug\":\"team-slug\",\"team_id\":\"team_id\"}\n"));
}

#[tokio::test]
async fn a_refresh_without_a_scope_keeps_the_saved_scope_and_reloads() {
    let server = refreshed(Reply::status(
        200,
        r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600,"token_type":"Bearer"}"#,
    ));
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &origin(&server), 1, TEAM);
    let key = login(&data)
        .load_credential(&CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key.secret.as_str(), "new-access");
    let saved = std::fs::read_to_string(data.join("auth.json")).unwrap();
    assert!(
        saved.contains(",\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\","),
        "{saved}"
    );
    let reloaded = login(&data)
        .load_credential(&CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.secret.as_str(), "new-access");
    assert_eq!(reloaded.gateway_team(), Some("team_id"));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn a_rejected_refresh_retires_the_session() {
    for (status, body, expected) in [
        (
            400,
            r#"{"error":"invalid_grant"}"#,
            VercelError::InvalidGrant,
        ),
        (
            200,
            r#"{"access_token":"new-access","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
            VercelError::NoRefreshToken,
        ),
        (200, "{}", VercelError::NoRefreshToken),
        (
            200,
            r#"{"access_token":"a","refresh_token":"r","expires_in":0,"token_type":"Bearer"}"#,
            VercelError::NoRefreshToken,
        ),
    ] {
        let server = refreshed(Reply::status(status, body));
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("oh-fx");
        write_session(&data, &origin(&server), 1, TEAM);
        let oauth = login(&data);
        assert_eq!(
            oauth.load_credential(&CancellationToken::new()).await.err(),
            Some(expected),
            "{body}"
        );
        assert_eq!(server.requests().len(), 2);
        assert_eq!(oauth.store().load_unlocked(), Ok(None), "{body}");
    }
}

#[tokio::test]
async fn a_temporary_refresh_failure_keeps_the_session() {
    for reply in [
        Reply::status(500, "not json"),
        Reply::status(200, "not json"),
        Reply::status(400, r#"{"error":"temporarily_unavailable"}"#),
    ] {
        let server = refreshed(reply);
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("oh-fx");
        write_session(&data, &origin(&server), 1, TEAM);
        let oauth = login(&data);
        assert_eq!(
            oauth.load_credential(&CancellationToken::new()).await.err(),
            Some(VercelError::OAuthRequestFailed)
        );
        assert!(oauth.store().load_unlocked().unwrap().is_some());
    }
}

#[tokio::test]
async fn a_refresh_that_cannot_be_saved_retires_the_consumed_session() {
    let gate = Gate::default();
    let server = FakeServer::start_at_origin(|origin| {
        [
            metadata(origin),
            Reply::status(
                200,
                r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
            )
            .after(&gate),
        ]
    });
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &origin(&server), 1, TEAM);
    let oauth = login(&data);
    let never = CancellationToken::new();
    let refresh = oauth.load_credential(&never);
    let release = async {
        while server.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        std::fs::set_permissions(
            data.join("auth.json"),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        gate.open();
    };
    let (result, ()) = tokio::join!(refresh, release);
    assert_eq!(
        result.err(),
        Some(VercelError::OAuthSessionCleanupUncertain)
    );
    assert!(!data.join("auth.json").exists());
}

#[tokio::test]
async fn an_unreadable_session_is_a_storage_failure_before_any_request() {
    let server = FakeServer::start([]);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &origin(&server), 1, TEAM);
    std::fs::set_permissions(
        data.join("auth.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let oauth = login(&data);
    assert_eq!(
        oauth.load_credential(&CancellationToken::new()).await.err(),
        Some(VercelError::CredentialStorageUnavailable)
    );
    assert_eq!(
        oauth.load_stored_credential().err(),
        Some(VercelError::CredentialStorageUnavailable)
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_cancelled_refresh_sends_nothing_before_the_token_request_and_keeps_the_session() {
    let server = FakeServer::start([]);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &origin(&server), 1, TEAM);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let oauth = login(&data);
    assert_eq!(
        oauth.load_credential(&cancel).await.err(),
        Some(VercelError::Cancelled)
    );
    assert!(server.requests().is_empty());
    assert!(oauth.store().load_unlocked().unwrap().is_some());
}

#[tokio::test]
async fn a_token_request_already_sent_is_answered_and_saved_after_cancellation() {
    let gate = Gate::default();
    let server = FakeServer::start_at_origin(|origin| {
        [
            metadata(origin),
            Reply::status(
                200,
                r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
            )
            .after(&gate),
        ]
    });
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &origin(&server), 1, TEAM);
    let oauth = login(&data);
    let cancel = CancellationToken::new();
    let refresh = oauth.load_credential(&cancel);
    let release = async {
        while server.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.open();
    };
    let (result, ()) = tokio::join!(refresh, release);
    assert_eq!(result.unwrap().unwrap().secret.as_str(), "new-access");
    let saved = std::fs::read_to_string(data.join("auth.json")).unwrap();
    assert!(
        saved.contains("\"refresh_token\":\"rotated-refresh\""),
        "{saved}"
    );
}

#[tokio::test]
async fn a_forced_refresh_renews_a_login_that_is_not_yet_due() {
    let server = refreshed(Reply::status(
        200,
        r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
    ));
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &origin(&server), i64::MAX, TEAM);
    let oauth = login(&data);
    let key = oauth
        .refresh_credential(&CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key.secret.as_str(), "new-access");
    assert_eq!(key.gateway_team(), Some("team_id"));
    assert_eq!(server.requests().len(), 2);
    let saved = std::fs::read_to_string(data.join("auth.json")).unwrap();
    assert!(
        saved.contains("\"refresh_token\":\"rotated-refresh\""),
        "{saved}"
    );
    let absent = tempfile::tempdir().unwrap();
    assert!(matches!(
        login(&absent.path().join("oh-fx"))
            .refresh_credential(&CancellationToken::new())
            .await,
        Ok(None)
    ));
}
