use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ofx_testkit::{FakeServer, Reply};

use super::*;
use crate::login_flow::VercelEndpoints;

fn write_session(directory: &Path, issuer: &str) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = directory.join("auth.json");
    std::fs::write(
        &file,
        format!(
            "{{\"version\":1,\"issuer\":\"{issuer}\",\"client_id\":\"client\",\"access_token\":\"access\",\"refresh_token\":\"refresh\",\"expires_at_ms\":1,\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\"}}"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn metadata(origin: &str, revocation: Option<&str>) -> Reply {
    let revocation = revocation.map_or_else(String::new, |endpoint| {
        format!(r#","revocation_endpoint":"{endpoint}""#)
    });
    Reply::status(
        200,
        format!(
            r#"{{"issuer":"{origin}","device_authorization_endpoint":"{origin}/device","token_endpoint":"{origin}/token"{revocation}}}"#
        ),
    )
}

fn oauth(directory: &Path) -> VercelOAuth {
    VercelOAuth::new(
        directory.to_owned(),
        "oh-fx/test",
        VercelEndpoints::default(),
    )
}

#[tokio::test]
async fn logout_deletes_the_session_and_revokes_both_tokens() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            metadata(origin, Some(&format!("{origin}/revoke"))),
            Reply::status(200, ""),
            Reply::status(200, ""),
        ]
    });
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &format!("http://{}", server.address()));
    let oauth = oauth(&data);
    assert_eq!(
        oauth.logout().await,
        Ok(LogoutResult {
            session_deleted: true,
            local_durability_failed: false,
            remote_revocation_failed: false,
        })
    );
    assert!(!data.join("auth.json").exists());
    let bodies: Vec<_> = server
        .requests()
        .iter()
        .skip(1)
        .map(ofx_testkit::RecordedRequest::body_text)
        .collect();
    assert_eq!(
        bodies,
        [
            "client_id=client&token=refresh&token_type_hint=refresh_token",
            "client_id=client&token=access&token_type_hint=access_token",
        ]
    );
    assert_eq!(oauth.logout().await, Ok(LogoutResult::default()));
}

#[tokio::test]
async fn logout_reports_a_revocation_it_could_not_confirm() {
    for (has_endpoint, replies) in [
        (false, vec![]),
        (true, vec![Reply::status(500, "{}"), Reply::status(200, "")]),
        (true, vec![Reply::status(200, ""), Reply::status(400, "{}")]),
    ] {
        let server = FakeServer::start_at_origin(|origin| {
            let endpoint = format!("{origin}/revoke");
            let mut all = vec![metadata(origin, has_endpoint.then_some(endpoint.as_str()))];
            all.extend(replies);
            all
        });
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("oh-fx");
        write_session(&data, &format!("http://{}", server.address()));
        let result = oauth(&data).logout().await.unwrap();
        assert!(result.session_deleted);
        assert!(result.remote_revocation_failed);
        assert!(!data.join("auth.json").exists());
        let expected_requests = if has_endpoint { 3 } else { 1 };
        assert_eq!(server.requests().len(), expected_requests);
    }
}

#[tokio::test]
async fn an_unreadable_session_is_deleted_without_revocation() {
    let server = FakeServer::start([]);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, "https://example.com");
    let result = oauth(&data).logout().await.unwrap();
    assert_eq!(
        result,
        LogoutResult {
            session_deleted: true,
            local_durability_failed: false,
            remote_revocation_failed: false,
        }
    );
    std::fs::write(data.join("auth.json"), "{}").unwrap();
    std::fs::set_permissions(
        data.join("auth.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let insecure = oauth(&data).logout().await.unwrap();
    assert!(insecure.session_deleted);
    assert!(insecure.remote_revocation_failed);
    assert!(server.requests().is_empty());
}
