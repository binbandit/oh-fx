use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use ofx_auth::VercelEndpoints;
use ofx_testkit::{FakeServer, Gate, Reply};

use super::*;

fn write_due_session(directory: &std::path::Path, issuer: &str) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = directory.join("auth.json");
    std::fs::write(
        &file,
        format!(
            "{{\"version\":1,\"issuer\":\"{issuer}\",\"client_id\":\"client\",\"access_token\":\"login-access\",\"refresh_token\":\"login-refresh\",\"expires_at_ms\":1,\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\"}}"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn a_refresh_dropped_by_a_cancelled_turn_still_saves_the_rotated_tokens() {
    let gate = Gate::default();
    let server = FakeServer::start_at_origin(|origin| {
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
            .after(&gate),
        ]
    });
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_due_session(&data, &format!("http://{}", server.address()));
    let refresh = LoginRefresh {
        login: VercelOAuth::new(data.clone(), "oh-fx/test", VercelEndpoints::default()),
    };
    let mut pending = refresh.refresh();
    let token_requested = async {
        while server.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        _ = &mut pending => panic!("the refresh finished before its token reply"),
        () = token_requested => {}
    }
    drop(pending);
    gate.open();
    let saved = data.join("auth.json");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !std::fs::read_to_string(&saved)
            .unwrap_or_default()
            .contains("\"refresh_token\":\"rotated-refresh\"")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the rotated refresh token is saved after the turn stops waiting");
}
