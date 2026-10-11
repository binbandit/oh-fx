use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use ofx_testkit::{FakeServer, Gate, Reply};

use super::*;
use crate::login_flow::VercelEndpoints;

const TEAMS: &str = r#"{"teams":[{"id":"team_one","slug":"one","name":"One"},{"id":"team_two","slug":"two","name":"Two"}]}"#;

fn write_session(directory: &Path, issuer: &str, expires_at_ms: i64) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = directory.join("auth.json");
    std::fs::write(
        &file,
        format!(
            "{{\"version\":1,\"issuer\":\"{issuer}\",\"client_id\":\"client\",\"access_token\":\"access\",\"refresh_token\":\"refresh\",\"expires_at_ms\":{expires_at_ms},\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\",\"team_slug\":\"two\",\"team_id\":\"team_two\"}}"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn oauth(directory: &Path) -> VercelOAuth {
    VercelOAuth::new(
        directory.to_owned(),
        "oh-fx/test",
        VercelEndpoints::default(),
    )
}

fn quiet() -> LoginTerminal {
    LoginTerminal {
        browser_prompt: false,
        team_picker: false,
    }
}

async fn switch(oauth: &VercelOAuth, typed: &[u8]) -> (Result<(), VercelError>, String) {
    let (input, mut writer) = UnixStream::pair().unwrap();
    writer.write_all(typed).unwrap();
    drop(writer);
    let mut output = Vec::new();
    let result = oauth.switch_team(&mut output, &input, quiet()).await;
    (result, String::from_utf8(output).unwrap())
}

#[tokio::test]
async fn switching_teams_offers_the_current_team_and_saves_the_choice() {
    let server = FakeServer::start([Reply::status(200, TEAMS)]);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &format!("http://{}", server.address()), i64::MAX);
    let oauth = oauth(&data);
    let (result, output) = switch(&oauth, b"1\n").await;
    assert_eq!(result, Ok(()));
    assert!(output.contains("  1. One (one)\n  2. Two (two) (default)\nTeam [2]: "));
    assert!(
        output.ends_with("Selected Vercel team: One (one).\n"),
        "{output}"
    );
    let saved = oauth.store().load_unlocked().unwrap().unwrap();
    assert_eq!(saved.team_id.as_deref(), Some("team_one"));
    assert_eq!(saved.team_slug.as_deref(), Some("one"));
    assert_eq!(saved.access_token.expose(), "access");
    assert_eq!(server.requests()[0].path, "/v2/teams");
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some("Bearer access")
    );
}

#[tokio::test]
async fn an_expired_session_is_refreshed_before_the_team_list() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            Reply::status(
                200,
                format!(
                    r#"{{"issuer":"{origin}","device_authorization_endpoint":"{origin}/device","token_endpoint":"{origin}/token"}}"#
                ),
            ),
            Reply::status(
                200,
                r#"{"access_token":"fresh","refresh_token":"rotated","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
            ),
            Reply::status(
                200,
                r#"{"teams":[{"id":"team_one","slug":"one","name":"One"}]}"#,
            ),
        ]
    });
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, &format!("http://{}", server.address()), 1);
    let oauth = oauth(&data);
    let (result, output) = switch(&oauth, b"").await;
    assert_eq!(result, Ok(()));
    assert_eq!(output, "Selected Vercel team: One (one).\n");
    assert_eq!(
        server.requests()[2].header("authorization"),
        Some("Bearer fresh")
    );
    let saved = oauth.store().load_unlocked().unwrap().unwrap();
    assert_eq!(saved.access_token.expose(), "fresh");
    assert_eq!(saved.team_id.as_deref(), Some("team_one"));
}

#[tokio::test]
async fn switching_teams_fails_without_a_session_teams_or_the_same_session() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    assert_eq!(
        switch(&oauth(&data), b"").await.0,
        Err(VercelError::NoSession)
    );

    for (reply, expected) in [
        (Reply::status(500, "{}"), VercelError::TeamRequestFailed),
        (Reply::status(200, r#"{"teams":[]}"#), VercelError::NoTeams),
        (Reply::status(200, "[]"), VercelError::InvalidTeamsResponse),
    ] {
        let server = FakeServer::start([reply]);
        write_session(&data, &format!("http://{}", server.address()), i64::MAX);
        assert_eq!(switch(&oauth(&data), b"").await.0, Err(expected));
    }

    let gate = Gate::default();
    let server = FakeServer::start([Reply::status(200, TEAMS).after(&gate)]);
    let issuer = format!("http://{}", server.address());
    write_session(&data, &issuer, i64::MAX);
    let oauth = oauth(&data);
    let changed = async {
        while server.requests().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        write_session(&data, &issuer, i64::MAX - 1);
        gate.open();
    };
    let ((result, _), ()) = tokio::join!(switch(&oauth, b"1\n"), changed);
    assert_eq!(result, Err(VercelError::SessionChanged));
    let kept = oauth.store().load_unlocked().unwrap().unwrap();
    assert_eq!(kept.team_id.as_deref(), Some("team_two"));
}
