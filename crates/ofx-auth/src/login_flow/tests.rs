use std::io::Write as _;
use std::os::unix::net::UnixStream;

use ofx_testkit::{FakeServer, Reply};

use super::*;
use crate::secret::Secret;

const ACCESS: &str = "granted-access-token";
const REFRESH: &str = "granted-refresh-token";

fn team(id: &str, slug: &str, name: &str) -> Team {
    Team {
        id: id.to_owned(),
        slug: slug.to_owned(),
        name: name.to_owned(),
    }
}

fn token(refresh: Option<&str>, expires_in: i64, scope: &str) -> TokenSet {
    TokenSet {
        access_token: Secret::new(ACCESS.to_owned()),
        refresh_token: refresh.map(|refresh| Secret::new(refresh.to_owned())),
        expires_in,
        scope: scope.to_owned(),
        token_type: "Bearer".to_owned(),
    }
}

fn metadata(origin: &str) -> String {
    format!(
        r#"{{"issuer":"{origin}","device_authorization_endpoint":"{origin}/oauth/device","token_endpoint":"{origin}/oauth/token","revocation_endpoint":"{origin}/oauth/revoke"}}"#
    )
}

fn device_reply() -> Reply {
    Reply::status(
        200,
        r#"{"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://vercel.com/device","verification_uri_complete":"https://vercel.com/device?code=ABCD-EFGH","expires_in":600,"interval":1}"#,
    )
}

fn token_reply() -> Reply {
    Reply::status(
        200,
        format!(
            r#"{{"access_token":"{ACCESS}","refresh_token":"{REFRESH}","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}}"#
        ),
    )
}

fn oauth(server: &FakeServer, directory: &std::path::Path, client_id: &str) -> VercelOAuth {
    VercelOAuth::new(
        directory.to_owned(),
        "oh-fx/test",
        VercelEndpoints {
            issuer: format!("http://{}", server.address()),
            client_id: client_id.to_owned(),
        },
    )
}

fn quiet() -> LoginTerminal {
    LoginTerminal {
        browser_prompt: false,
        team_picker: false,
    }
}

#[test]
fn login_flow_parses_teams() {
    let teams = parse_teams(
        br#"{"teams":[{"id":"team_1","slug":"vercel-labs","name":"Vercel Labs"},{"id":"team_2","slug":"personal"},{"id":"team_3","slug":"blank","name":""},{"id":7,"slug":"skipped"},"skipped"]}"#,
    )
    .unwrap();
    assert_eq!(
        teams,
        [
            team("team_1", "vercel-labs", "Vercel Labs"),
            team("team_2", "personal", "personal"),
            team("team_3", "blank", "blank"),
        ]
    );
    for invalid in [&b"[]"[..], br#"{"teams":{}}"#, b"{}", b"not json"] {
        assert_eq!(parse_teams(invalid), Err(VercelError::InvalidTeamsResponse));
    }
}

#[test]
fn team_selection_needs_no_picker_for_zero_or_one_team() {
    let (input, _writer) = UnixStream::pair().unwrap();
    let mut output = Vec::new();
    assert_eq!(select_team(&[], None, true, &input, &mut output), Ok(None));
    let one = [team("team_1", "one", "One")];
    assert_eq!(
        select_team(&one, None, true, &input, &mut output),
        Ok(Some(0))
    );
    assert!(output.is_empty());
}

#[test]
fn the_default_team_is_the_current_id_or_slug() {
    let teams = [team("team_1", "one", "One"), team("team_2", "two", "Two")];
    assert_eq!(default_team_index(&teams, None), 0);
    assert_eq!(default_team_index(&teams, Some("team_2")), 1);
    assert_eq!(default_team_index(&teams, Some("two")), 1);
    assert_eq!(default_team_index(&teams, Some("missing")), 0);
}

#[test]
fn the_login_session_takes_the_granted_tokens_and_team() {
    let selected = team("i", "s", "n");
    let session = take_login_session(
        "https://vercel.com",
        "client",
        token(Some(REFRESH), 3600, "openid offline_access"),
        Some(&selected),
        1_000,
    )
    .unwrap();
    assert_eq!(session.access_token.expose(), ACCESS);
    assert_eq!(session.refresh_token.expose(), REFRESH);
    assert_eq!(session.expires_at_ms, 3_601_000);
    assert_eq!(
        (session.team_id.as_deref(), session.team_slug.as_deref()),
        (Some("i"), Some("s"))
    );
    assert_eq!(
        take_login_session(
            "https://vercel.com",
            "client",
            token(None, 3600, ""),
            None,
            0
        ),
        Err(VercelError::NoRefreshToken)
    );
    assert_eq!(
        take_login_session(
            "https://vercel.com",
            "client",
            token(Some(REFRESH), 0, ""),
            None,
            0
        ),
        Err(VercelError::InvalidOAuthResponse)
    );
}

#[test]
fn the_login_browser_opener_respects_environment_tty_and_platform_gates() {
    assert!(!browser_open_enabled(true, true, true));
    assert!(!browser_open_enabled(false, false, true));
    assert!(!browser_open_enabled(false, true, false));
    assert!(browser_open_enabled(false, true, true));
}

#[tokio::test]
async fn run_login_signs_in_saves_the_session_and_keeps_tokens_out_of_the_output() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            Reply::status(200, metadata(origin)),
            device_reply(),
            token_reply(),
            Reply::status(
                200,
                r#"{"teams":[{"id":"team_1","slug":"one","name":"One"},{"id":"team_2","slug":"two","name":"Two"}]}"#,
            ),
        ]
    });
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth(&server, &directory.path().join("oh-fx"), "client");
    let (input, mut writer) = UnixStream::pair().unwrap();
    writer.write_all(b"2\n").unwrap();
    let mut output = Vec::new();
    oauth.run_login(&mut output, &input, quiet()).await.unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(
        output.starts_with(
            "Open https://vercel.com/device?code=ABCD-EFGH\nCode: ABCD-EFGH\n\nWaiting for authentication...\n\nSelect a Vercel team for AI Gateway:\n"
        ),
        "{output}"
    );
    assert!(output.ends_with("Team [1]: "), "{output}");
    for secret in [ACCESS, REFRESH, "device-secret"] {
        assert!(!output.contains(secret));
    }
    let requests = server.requests();
    let paths: Vec<_> = requests
        .iter()
        .map(|request| format!("{} {}", request.method, request.path))
        .collect();
    assert_eq!(
        paths,
        [
            "GET /.well-known/openid-configuration",
            "POST /oauth/device",
            "POST /oauth/token",
            "GET /v2/teams",
        ]
    );
    assert_eq!(
        requests[3].header("authorization"),
        Some(format!("Bearer {ACCESS}").as_str())
    );
    let saved = oauth.store().load_unlocked().unwrap().unwrap();
    assert_eq!(saved.issuer, format!("http://{}", server.address()));
    assert_eq!(saved.client_id, "client");
    assert_eq!(saved.team_id.as_deref(), Some("team_2"));
    assert_eq!(saved.team_slug.as_deref(), Some("two"));
}

#[tokio::test]
async fn run_login_falls_back_to_the_default_client_id_once() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            Reply::status(200, metadata(origin)),
            Reply::status(401, r#"{"error":"invalid_client"}"#),
            device_reply(),
            token_reply(),
            Reply::status(500, "{}"),
        ]
    });
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth(&server, &directory.path().join("oh-fx"), "cl_custom");
    let (input, _writer) = UnixStream::pair().unwrap();
    oauth
        .run_login(&mut Vec::new(), &input, quiet())
        .await
        .unwrap();
    let requests = server.requests();
    assert_eq!(
        requests[1].body_text(),
        "client_id=cl_custom&scope=openid%20offline_access"
    );
    assert_eq!(
        requests[2].body_text(),
        format!(
            "client_id={}&scope=openid%20offline_access",
            oauth_session::DEFAULT_CLIENT_ID
        )
    );
    let saved = oauth.store().load_unlocked().unwrap().unwrap();
    assert_eq!(saved.client_id, oauth_session::DEFAULT_CLIENT_ID);
    assert_eq!(saved.team_id, None);
}

#[tokio::test]
async fn run_login_refuses_endpoints_outside_the_loopback_issuer() {
    let server = FakeServer::start_at_origin(|origin| {
        [Reply::status(
            200,
            metadata(origin).replace(&format!("{origin}/oauth/token"), "https://vercel.com/token"),
        )]
    });
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth(&server, &directory.path().join("oh-fx"), "client");
    let (input, _writer) = UnixStream::pair().unwrap();
    assert_eq!(
        oauth.run_login(&mut Vec::new(), &input, quiet()).await,
        Err(VercelError::InvalidE2EOAuthEndpoint)
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn run_login_reports_a_denied_authorization() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            Reply::status(200, metadata(origin)),
            device_reply(),
            Reply::status(400, r#"{"error":"access_denied"}"#),
        ]
    });
    let directory = tempfile::tempdir().unwrap();
    let oauth = oauth(&server, &directory.path().join("oh-fx"), "client");
    let (input, _writer) = UnixStream::pair().unwrap();
    assert_eq!(
        oauth.run_login(&mut Vec::new(), &input, quiet()).await,
        Err(VercelError::AccessDenied)
    );
    assert_eq!(oauth.store().load_unlocked(), Ok(None));
}
