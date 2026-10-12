use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, Reply};
use serde_json::{Value, json};

const ACCESS: &str = "vercel-access-token-0123456789";
const REFRESH: &str = "vercel-refresh-token-0123456789";
const ROTATED_ACCESS: &str = "vercel-rotated-access-0123456789";
const ROTATED_REFRESH: &str = "vercel-rotated-refresh-0123456789";
const DEVICE_CODE: &str = "vercel-device-code-0123456789";
const CHAT_PATH: &str = "/v4/ai/language-model";
const SIGNED_IN: &str = "Signed in to Vercel.\nAI Gateway access may still require billing or API setup for the selected account.\n";
const SECRETS: [&str; 5] = [
    ACCESS,
    REFRESH,
    ROTATED_ACCESS,
    ROTATED_REFRESH,
    DEVICE_CODE,
];

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory
            .path()
            .canonicalize()
            .expect("canonicalize the home");
        fs::create_dir_all(root.join("workspace")).expect("create the workspace");
        Self {
            _directory: directory,
            root,
        }
    }

    fn data(&self) -> PathBuf {
        self.root.join("data/oh-fx")
    }

    fn session_file(&self) -> PathBuf {
        self.data().join("auth.json")
    }

    fn settings(&self) -> Option<Value> {
        fs::read_to_string(self.root.join("config/oh-fx/settings.json"))
            .ok()
            .map(|text| serde_json::from_str(&text).expect("settings JSON"))
    }

    fn trace_log(&self) -> PathBuf {
        self.root.join("trace.log")
    }

    fn write_session(&self, issuer: &str, expires_at_ms: i64) {
        fs::create_dir_all(self.data()).expect("create the data directory");
        fs::set_permissions(self.data(), fs::Permissions::from_mode(0o700))
            .expect("make the data directory private");
        let session = format!(
            "{{\"version\":1,\"issuer\":\"{issuer}\",\"client_id\":\"cl_test\",\"access_token\":\"{ACCESS}\",\"refresh_token\":\"{REFRESH}\",\"expires_at_ms\":{expires_at_ms},\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\",\"team_slug\":\"labs\",\"team_id\":\"team_labs\"}}\n"
        );
        fs::write(self.session_file(), session).expect("write the session");
        fs::set_permissions(self.session_file(), fs::Permissions::from_mode(0o600))
            .expect("make the session private");
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)], input: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("OH_FX_NO_OPEN_BROWSER", "1")
            .env("OH_FX_TRACE_LOG", self.trace_log())
            .envs(environment.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run oh-fx");
        let mut stdin = child.stdin.take().expect("child stdin");
        stdin.write_all(input).expect("write stdin");
        drop(stdin);
        child.wait_with_output().expect("wait for oh-fx")
    }

    fn assert_no_secrets(&self, output: &Output) {
        let trace = fs::read_to_string(self.trace_log()).unwrap_or_default();
        for secret in SECRETS {
            assert!(
                !text(&output.stdout).contains(secret),
                "stdout has {secret}"
            );
            assert!(
                !text(&output.stderr).contains(secret),
                "stderr has {secret}"
            );
            assert!(!trace.contains(secret), "trace has {secret}");
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn metadata(origin: &str) -> Reply {
    Reply::status(
        200,
        json!({
            "issuer": origin,
            "device_authorization_endpoint": format!("{origin}/oauth/device"),
            "token_endpoint": format!("{origin}/oauth/token"),
            "revocation_endpoint": format!("{origin}/oauth/revoke"),
        })
        .to_string(),
    )
}

fn device() -> Reply {
    Reply::status(
        200,
        json!({
            "device_code": DEVICE_CODE,
            "user_code": "WXYZ-1234",
            "verification_uri": "https://vercel.com/oauth/device",
            "verification_uri_complete": "https://vercel.com/oauth/device?user_code=WXYZ-1234",
            "expires_in": 600,
            "interval": 1,
        })
        .to_string(),
    )
}

fn token(access: &str, refresh: &str) -> Reply {
    Reply::status(
        200,
        json!({
            "access_token": access,
            "refresh_token": refresh,
            "expires_in": 3600,
            "scope": "openid offline_access",
            "token_type": "Bearer",
        })
        .to_string(),
    )
}

fn teams() -> Reply {
    Reply::status(
        200,
        json!({"teams": [
            {"id": "team_personal", "slug": "personal", "name": "Personal"},
            {"id": "team_labs", "slug": "labs", "name": "Labs"},
        ]})
        .to_string(),
    )
}

fn chat(text: &str) -> Reply {
    Reply::sse(&[
        json!({"type": "text-delta", "id": "t", "delta": text}).to_string(),
        r#"{"type":"finish","finishReason":{"unified":"stop"},"usage":{"inputTokens":{"total":1},"outputTokens":{"total":1}}}"#.to_owned(),
    ])
}

fn origin(server: &FakeServer) -> String {
    format!("http://{}", server.address())
}

#[test]
fn login_signs_in_chooses_a_team_and_selects_the_gateway_with_the_login() {
    let server = FakeServer::start_at_origin(|origin| {
        [metadata(origin), device(), token(ACCESS, REFRESH), teams()]
    });
    let home = Home::new();
    let issuer = origin(&server);
    let output = home.run(
        &["login"],
        &[
            ("OH_FX_E2E_OAUTH_ISSUER_URL", &issuer),
            ("OH_FX_OAUTH_CLIENT_ID", "cl_test"),
        ],
        b"2\n",
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!(
            "Open https://vercel.com/oauth/device?user_code=WXYZ-1234\nCode: WXYZ-1234\n\nWaiting for authentication...\n\nSelect a Vercel team for AI Gateway:\nModel requests will use the selected team; Gateway access may still require billing or API setup.\n\n  1. Personal (personal) (default)\n  2. Labs (labs)\nTeam [1]: {SIGNED_IN}"
        )
    );
    assert_eq!(text(&output.stderr), "");
    home.assert_no_secrets(&output);
    let requests = server.requests();
    assert_eq!(
        requests[1].body_text(),
        "client_id=cl_test&scope=openid%20offline_access"
    );
    assert_eq!(
        requests[3].header("authorization"),
        Some(format!("Bearer {ACCESS}").as_str())
    );
    let session = fs::read_to_string(home.session_file()).expect("a saved session");
    assert!(session.starts_with(&format!(
        "{{\"version\":1,\"issuer\":\"{issuer}\",\"client_id\":\"cl_test\",\"access_token\":\"{ACCESS}\",\"refresh_token\":\"{REFRESH}\",\"expires_at_ms\":"
    )));
    assert!(session.ends_with(",\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\",\"team_slug\":\"labs\",\"team_id\":\"team_labs\"}\n"));
    assert_eq!(
        fs::metadata(home.session_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        home.settings(),
        Some(json!({
            "credential_source": "fx_login",
            "models": {"gateway": "spacexai/grok-4.7"},
            "provider": "gateway",
        }))
    );

    let status = home.run(&["status"], &[], b"");
    let shown = text(&status.stdout);
    assert!(
        shown.contains(
            "\n[status] auth=oh-fx login\n[status] auth_refreshable=true\n[status] team=labs\n"
        ),
        "{shown}"
    );
    home.assert_no_secrets(&status);
}

#[test]
fn ask_sends_the_login_token_and_its_team_ahead_of_an_environment_key() {
    let server = FakeServer::start([chat("hello")]);
    let home = Home::new();
    home.write_session("https://vercel.com", i64::MAX);
    fs::create_dir_all(home.root.join("config/oh-fx")).unwrap();
    fs::write(
        home.root.join("config/oh-fx/settings.json"),
        r#"{"credential_source":"fx_login"}"#,
    )
    .unwrap();
    let url = format!("{}{CHAT_PATH}", origin(&server));
    let output = home.run(
        &["ask", "--no-save", "hi"],
        &[
            ("OH_FX_E2E_GATEWAY_CHAT_URL", &url),
            ("AI_GATEWAY_API_KEY", "vck-environment-key-0123456789"),
        ],
        b"",
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "hello");
    let request = &server.requests()[0];
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {ACCESS}").as_str())
    );
    assert_eq!(
        request.header("x-vercel-ai-gateway-team"),
        Some("team_labs")
    );
    home.assert_no_secrets(&output);
}

#[test]
fn ask_refreshes_an_expired_login_before_the_request() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            metadata(origin),
            token(ROTATED_ACCESS, ROTATED_REFRESH),
            chat("fresh"),
        ]
    });
    let home = Home::new();
    let issuer = origin(&server);
    home.write_session(&issuer, 1);
    let url = format!("{issuer}{CHAT_PATH}");
    let output = home.run(
        &["ask", "--no-save", "hi"],
        &[("OH_FX_E2E_GATEWAY_CHAT_URL", &url)],
        b"",
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    let requests = server.requests();
    assert_eq!(
        requests[1].body_text(),
        format!("client_id=cl_test&grant_type=refresh_token&refresh_token={REFRESH}")
    );
    assert_eq!(
        requests[2].header("authorization"),
        Some(format!("Bearer {ROTATED_ACCESS}").as_str())
    );
    assert_eq!(
        requests[2].header("x-vercel-ai-gateway-team"),
        Some("team_labs")
    );
    let session = fs::read_to_string(home.session_file()).unwrap();
    assert!(session.contains(&format!(
        "\"access_token\":\"{ROTATED_ACCESS}\",\"refresh_token\":\"{ROTATED_REFRESH}\""
    )));
    assert!(session.contains("\"team_id\":\"team_labs\""));
    home.assert_no_secrets(&output);
}

#[test]
fn ask_refreshes_a_rejected_login_once_and_replays_the_request() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            Reply::status(
                401,
                format!(r#"{{"error":{{"message":"token {ACCESS} expired"}}}}"#),
            ),
            metadata(origin),
            token(ROTATED_ACCESS, ROTATED_REFRESH),
            chat("replayed"),
        ]
    });
    let home = Home::new();
    let issuer = origin(&server);
    home.write_session(&issuer, i64::MAX);
    let url = format!("{issuer}{CHAT_PATH}");
    let output = home.run(
        &["ask", "--no-save", "hi"],
        &[("OH_FX_E2E_GATEWAY_CHAT_URL", &url), ("OH_FX_TRACE", "1")],
        b"",
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "replayed");
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {ACCESS}").as_str())
    );
    assert_eq!(
        requests[2].body_text(),
        format!("client_id=cl_test&grant_type=refresh_token&refresh_token={REFRESH}")
    );
    assert_eq!(
        requests[3].header("authorization"),
        Some(format!("Bearer {ROTATED_ACCESS}").as_str())
    );
    assert_eq!(requests[0].body_text(), requests[3].body_text());
    let session = fs::read_to_string(home.session_file()).unwrap();
    assert!(session.contains(&format!("\"refresh_token\":\"{ROTATED_REFRESH}\"")));
    let trace = fs::read_to_string(home.trace_log()).unwrap_or_default();
    assert!(trace.contains("authenticated_request_replayed"), "{trace}");
    home.assert_no_secrets(&output);
}

#[test]
fn a_rejected_refresh_retires_the_login_and_asks_for_a_new_sign_in() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            metadata(origin),
            Reply::status(400, r#"{"error":"invalid_grant"}"#),
        ]
    });
    let home = Home::new();
    home.write_session(&origin(&server), 1);
    let output = home.run(&["ask", "--no-save", "hi"], &[], b"");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "oh-fx ask: oh-fx needs access to Vercel AI Gateway. Run oh-fx login to sign in, oh-fx setup to use an API key, or set AI_GATEWAY_API_KEY.\n"
    );
    assert!(!home.session_file().exists());
    home.assert_no_secrets(&output);
}

#[test]
fn a_denied_or_expired_authorization_fails_the_login_without_saving() {
    for (reply, detail) in [
        (
            Reply::status(400, r#"{"error":"access_denied"}"#),
            "authorization denied",
        ),
        (
            Reply::status(400, r#"{"error":"expired_token"}"#),
            "authorization expired; run oh-fx login again",
        ),
    ] {
        let server = FakeServer::start_at_origin(|origin| [metadata(origin), device(), reply]);
        let home = Home::new();
        let issuer = origin(&server);
        let output = home.run(
            &["login", "vercel"],
            &[("OH_FX_E2E_OAUTH_ISSUER_URL", &issuer)],
            b"",
        );
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(text(&output.stderr), format!("oh-fx login: {detail}\n"));
        assert!(!home.session_file().exists());
        assert_eq!(home.settings(), None);
        let trace = fs::read_to_string(home.trace_log()).unwrap_or_default();
        assert!(
            trace.contains("provider sign-in failed provider=gateway err="),
            "{trace}"
        );
        home.assert_no_secrets(&output);
    }
}

#[test]
fn a_login_without_a_team_is_saved_but_does_not_select_the_gateway() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            metadata(origin),
            device(),
            token(ACCESS, REFRESH),
            Reply::status(403, "{}"),
        ]
    });
    let home = Home::new();
    let issuer = origin(&server);
    let output = home.run(&["login"], &[("OH_FX_E2E_OAUTH_ISSUER_URL", &issuer)], b"");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "oh-fx login: configure a Gateway credential first\n"
    );
    assert!(home.session_file().exists());
    assert_eq!(home.settings(), None);
    home.assert_no_secrets(&output);
}

#[test]
fn doctor_reports_an_expired_login_with_its_team_without_refreshing_it() {
    let server = FakeServer::start([]);
    let home = Home::new();
    home.write_session(&origin(&server), 1);
    let output = home.run(&["doctor"], &[], b"");
    let shown = text(&output.stdout);
    assert!(
        shown.contains("\n[doctor] auth=oh-fx login\n[doctor] auth_refreshable=true\n[doctor] auth_expired=true\n[doctor] team=labs\n[doctor] permission_mode="),
        "{shown}"
    );
    assert!(
        shown.contains(
            "\n[warn] auth: oh-fx login is configured; session expired; refreshable=true; team=labs\n"
        ),
        "{shown}"
    );
    let json = home.run(&["doctor", "--json"], &[], b"");
    let report: Value = serde_json::from_slice(&json.stdout).expect("doctor JSON");
    assert_eq!(report["auth"], "oh-fx login");
    assert_eq!(report["auth_expired"], true);
    assert_eq!(report["team"], "labs");
    assert!(server.requests().is_empty());
    home.assert_no_secrets(&output);
    home.assert_no_secrets(&json);
}

#[test]
fn teams_switches_the_saved_team_and_selects_the_gateway_with_the_login() {
    let server = FakeServer::start([teams()]);
    let home = Home::new();
    home.write_session(&origin(&server), i64::MAX);
    let output = home.run(&["teams"], &[], b"1\n");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "\nSelect a Vercel team for AI Gateway:\nModel requests will use the selected team; Gateway access may still require billing or API setup.\n\n  1. Personal (personal)\n  2. Labs (labs) (default)\nTeam [2]: Selected Vercel team: Personal (personal).\n"
    );
    assert_eq!(text(&output.stderr), "");
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some(format!("Bearer {ACCESS}").as_str())
    );
    let session = fs::read_to_string(home.session_file()).unwrap();
    assert!(session.ends_with(",\"team_slug\":\"personal\",\"team_id\":\"team_personal\"}\n"));
    assert_eq!(
        home.settings(),
        Some(json!({
            "credential_source": "fx_login",
            "models": {"gateway": "spacexai/grok-4.7"},
            "provider": "gateway",
        }))
    );
    home.assert_no_secrets(&output);
}

#[test]
fn teams_needs_a_login_and_a_team_list() {
    let home = Home::new();
    let output = home.run(&["teams"], &[], b"");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stderr), "oh-fx teams: run oh-fx login first\n");

    let server = FakeServer::start([Reply::status(500, "{}")]);
    home.write_session(&origin(&server), i64::MAX);
    let output = home.run(&["teams"], &[], b"");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "oh-fx teams: failed to list Vercel teams\n"
    );
    assert!(
        fs::read_to_string(home.session_file())
            .unwrap()
            .contains("\"team_id\":\"team_labs\"")
    );
    home.assert_no_secrets(&output);
}

#[test]
fn logout_revokes_the_login_removes_it_and_forgets_the_chosen_source() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            metadata(origin),
            Reply::status(200, ""),
            Reply::status(200, ""),
        ]
    });
    let home = Home::new();
    home.write_session(&origin(&server), i64::MAX);
    fs::create_dir_all(home.root.join("config/oh-fx")).unwrap();
    fs::write(
        home.root.join("config/oh-fx/settings.json"),
        r#"{"provider":"gateway","credential_source":"fx_login"}"#,
    )
    .unwrap();
    let output = home.run(&["logout"], &[], b"");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "Signed out of oh-fx.\n");
    assert_eq!(text(&output.stderr), "");
    assert!(!home.session_file().exists());
    assert_eq!(home.settings(), Some(json!({"provider": "gateway"})));
    let bodies: Vec<String> = server
        .requests()
        .iter()
        .skip(1)
        .map(ofx_testkit::RecordedRequest::body_text)
        .collect();
    assert_eq!(
        bodies,
        [
            format!("client_id=cl_test&token={REFRESH}&token_type_hint=refresh_token"),
            format!("client_id=cl_test&token={ACCESS}&token_type_hint=access_token"),
        ]
    );
    home.assert_no_secrets(&output);

    let again = home.run(&["logout", "vercel"], &[], b"");
    assert!(again.status.success());
    assert_eq!(text(&again.stdout), "No oh-fx login session found.\n");
}

#[test]
fn logout_warns_when_the_remote_session_could_not_be_revoked() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            metadata(origin),
            Reply::status(500, "{}"),
            Reply::status(200, ""),
        ]
    });
    let home = Home::new();
    home.write_session(&origin(&server), i64::MAX);
    let output = home.run(&["logout"], &[], b"");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "Signed out of oh-fx.\n");
    assert_eq!(
        text(&output.stderr),
        "Warning: signed out locally, but the remote session could not be revoked.\n"
    );
    assert!(!home.session_file().exists());
    home.assert_no_secrets(&output);
}

#[test]
fn host_managed_authentication_leaves_teams_and_logout_alone() {
    let server = FakeServer::start([]);
    let home = Home::new();
    home.write_session(&origin(&server), i64::MAX);
    for args in [&["teams"][..], &["logout"], &["logout", "vercel"]] {
        let output = home.run(args, &[("OH_FX_AUTH_MODE", "host-managed")], b"");
        assert!(output.status.success(), "{args:?}");
        assert_eq!(
            text(&output.stdout),
            "Authentication is managed by the host.\n"
        );
    }
    assert!(home.session_file().exists());
    assert!(server.requests().is_empty());
}

#[test]
fn teams_saves_the_team_before_an_activation_failure() {
    let server = FakeServer::start([teams()]);
    let home = Home::new();
    home.write_session(&origin(&server), i64::MAX);
    fs::create_dir_all(home.root.join("config/oh-fx")).unwrap();
    fs::write(home.root.join("config/oh-fx/settings.json"), "{").unwrap();
    let output = home.run(&["teams"], &[], b"1\n");
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stdout).ends_with("Selected Vercel team: Personal (personal).\n"));
    assert_eq!(
        text(&output.stderr),
        "oh-fx login: could not load settings\n"
    );
    assert!(
        fs::read_to_string(home.session_file())
            .unwrap()
            .contains("\"team_id\":\"team_personal\"")
    );
    home.assert_no_secrets(&output);
}
