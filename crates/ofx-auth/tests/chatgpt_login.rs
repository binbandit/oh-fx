use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ofx_auth::{
    ChatGptEndpoints, ChatGptError, ChatGptOAuth, DeleteOutcome, PreparationError, RefreshMode,
    prepare_chatgpt_credential, refresh_chatgpt_credential,
};
use ofx_testkit::{FakeServer, Reply};
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

const REFRESH_TOKEN: &str = "rt-refresh-secret-0123456789";

struct Forward(Sender<Vec<u8>>);

impl Write for Forward {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.0.send(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("test fixture step succeeds");
        let data = home.path().join("data/oh-fx");
        Self { _home: home, data }
    }

    fn oauth(&self, server: &FakeServer) -> ChatGptOAuth {
        ChatGptOAuth::new(
            self.data.clone(),
            "oh-fx/test",
            ChatGptEndpoints {
                issuer: server.base_url(),
                token_url: format!("{}/oauth/token", server.base_url()),
                callback_ports: vec![0],
            },
        )
        .expect("test fixture step succeeds")
    }

    fn credential_file(&self) -> PathBuf {
        self.data.join("chatgpt-auth.json")
    }

    fn write_session(&self, access_token: &str, expires_at_ms: i64) {
        std::fs::create_dir_all(&self.data).expect("test fixture step succeeds");
        std::fs::set_permissions(&self.data, std::fs::Permissions::from_mode(0o700))
            .expect("test fixture step succeeds");
        let session = json!({
            "version": 1,
            "access_token": access_token,
            "refresh_token": REFRESH_TOKEN,
            "expires_at_ms": expires_at_ms,
            "account_id": "acct_test",
        });
        std::fs::write(self.credential_file(), format!("{session}\n"))
            .expect("test fixture step succeeds");
        std::fs::set_permissions(
            self.credential_file(),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("test fixture step succeeds");
    }

    fn saved(&self) -> Value {
        serde_json::from_slice(
            &std::fs::read(self.credential_file()).expect("test fixture step succeeds"),
        )
        .expect("test fixture step succeeds")
    }
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test fixture step succeeds")
            .as_millis(),
    )
    .expect("test fixture step succeeds")
}

fn access_token(account: &str, marker: &str) -> String {
    let claims = json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": account},
        "exp": 4_102_444_800_i64,
        "marker": marker,
    });
    format!(
        "eyJhbGciOiJub25lIn0.{}.c2lnbmF0dXJl",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

fn token_reply(access: &str) -> Reply {
    Reply::status(
        200,
        json!({"access_token": access, "refresh_token": REFRESH_TOKEN, "expires_in": 3600})
            .to_string(),
    )
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("test fixture step succeeds")
        .permissions()
        .mode()
        & 0o777
}

fn read_until_waiting(receiver: &Receiver<Vec<u8>>) -> String {
    let mut printed = Vec::new();
    while !String::from_utf8_lossy(&printed).ends_with("Waiting for browser authorization...\n") {
        printed.extend(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("test fixture step succeeds"),
        );
    }
    String::from_utf8(printed).expect("test fixture step succeeds")
}

fn get(port: u16, target: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("test fixture step succeeds");
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes())
        .expect("test fixture step succeeds");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("test fixture step succeeds");
    response
}

struct Authorization {
    printed: String,
    state: String,
    challenge: String,
    redirect_uri: String,
    port: u16,
}

async fn begin_login(
    oauth: ChatGptOAuth,
) -> (
    Authorization,
    tokio::task::JoinHandle<Result<(), ChatGptError>>,
) {
    let (sender, receiver) = mpsc::channel();
    let login = tokio::spawn(async move {
        let mut output = Forward(sender);
        oauth
            .run_login(&mut output, false, &CancellationToken::new())
            .await
    });
    let printed = tokio::task::spawn_blocking(move || read_until_waiting(&receiver))
        .await
        .expect("test fixture step succeeds");
    let url = Url::parse(printed.lines().nth(1).expect("test fixture step succeeds"))
        .expect("test fixture step succeeds");
    let query = |key: &str| {
        url.query_pairs()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.into_owned())
            .expect("test fixture step succeeds")
    };
    let redirect_uri = query("redirect_uri");
    let port = Url::parse(&redirect_uri)
        .expect("test fixture step succeeds")
        .port()
        .expect("test fixture step succeeds");
    (
        Authorization {
            state: query("state"),
            challenge: query("code_challenge"),
            redirect_uri,
            port,
            printed,
        },
        login,
    )
}

async fn browser(port: u16, target: String) -> String {
    tokio::task::spawn_blocking(move || get(port, &target))
        .await
        .expect("test fixture step succeeds")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_via_the_browser_callback_stores_a_private_session() {
    let access = access_token("acct_test", "login");
    let server = FakeServer::start([token_reply(&access)]);
    let fixture = Fixture::new();
    let (authorization, login) = begin_login(fixture.oauth(&server)).await;
    assert!(
        authorization
            .printed
            .starts_with("Open this URL to sign in with Codex:\n")
    );
    assert!(authorization.redirect_uri.starts_with("http://localhost:"));
    assert!(authorization.redirect_uri.ends_with("/auth/callback"));

    let forged = browser(
        authorization.port,
        "/auth/callback?code=forged&state=not-the-state".to_owned(),
    )
    .await;
    assert!(forged.starts_with("HTTP/1.1 404 Not Found\r\n"));
    let other_path = browser(
        authorization.port,
        format!("/other?code=forged&state={}", authorization.state),
    )
    .await;
    assert!(other_path.starts_with("HTTP/1.1 404 Not Found\r\n"));

    let page = browser(
        authorization.port,
        format!(
            "/auth/callback?code=auth-code&state={}",
            authorization.state
        ),
    )
    .await;
    assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
    assert!(page.contains("Returning you to oh-fx."));
    login.await.unwrap().unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/oauth/token");
    assert_eq!(
        requests[0].header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    let form: Vec<(String, String)> = Url::parse(&format!("http://x/?{}", requests[0].body_text()))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    let names: Vec<&str> = form.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "grant_type",
            "client_id",
            "code",
            "code_verifier",
            "redirect_uri"
        ]
    );
    assert_eq!(form[0].1, "authorization_code");
    assert_eq!(form[2].1, "auth-code");
    assert_eq!(form[4].1, authorization.redirect_uri);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(form[3].1.as_bytes())),
        authorization.challenge
    );

    assert_eq!(mode(&fixture.data), 0o700);
    assert_eq!(mode(&fixture.credential_file()), 0o600);
    let saved = fixture.saved();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["access_token"], access);
    assert_eq!(saved["refresh_token"], REFRESH_TOKEN);
    assert_eq!(saved["account_id"], "acct_test");
    assert!(saved["expires_at_ms"].as_i64().unwrap() > now_ms());
    assert!(!authorization.printed.contains(&access));
    assert!(!authorization.printed.contains(REFRESH_TOKEN));
    assert!(!page.contains(&access));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_authorization_fails_without_storing_anything() {
    let server = FakeServer::start([]);
    let fixture = Fixture::new();
    let (authorization, login) = begin_login(fixture.oauth(&server)).await;
    let page = browser(
        authorization.port,
        format!(
            "/auth/callback?error=access_denied&state={}",
            authorization.state
        ),
    )
    .await;
    assert!(page.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        login.await.unwrap().unwrap_err(),
        ChatGptError::ChatGptAuthorizationFailed
    );
    assert!(server.requests().is_empty());
    assert!(!fixture.credential_file().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_code_exchange_reports_failure_to_the_browser() {
    let server = FakeServer::start([Reply::status(400, r#"{"error":"invalid_grant"}"#)]);
    let fixture = Fixture::new();
    let (authorization, login) = begin_login(fixture.oauth(&server)).await;
    let page = browser(
        authorization.port,
        format!("/auth/callback?code=stale&state={}", authorization.state),
    )
    .await;
    assert!(page.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(
        login.await.unwrap().unwrap_err(),
        ChatGptError::ChatGptOAuthRequestFailed
    );
    assert!(!fixture.credential_file().exists());
}

#[tokio::test(start_paused = true)]
async fn an_abandoned_login_times_out() {
    let server = FakeServer::start([]);
    let fixture = Fixture::new();
    let mut output = Vec::new();
    assert_eq!(
        fixture
            .oauth(&server)
            .run_login(&mut output, false, &CancellationToken::new())
            .await
            .unwrap_err(),
        ChatGptError::LoginTimedOut
    );
}

#[tokio::test]
async fn a_cancelled_login_stops_waiting() {
    let server = FakeServer::start([]);
    let fixture = Fixture::new();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut output = Vec::new();
    assert_eq!(
        fixture
            .oauth(&server)
            .run_login(&mut output, false, &cancel)
            .await
            .unwrap_err(),
        ChatGptError::Cancelled
    );
}

#[tokio::test]
async fn logout_removes_the_saved_session() {
    let server = FakeServer::start([]);
    let fixture = Fixture::new();
    let oauth = fixture.oauth(&server);
    assert_eq!(oauth.logout().await.unwrap(), DeleteOutcome::Missing);
    fixture.write_session(&access_token("acct_test", "logout"), now_ms() + 3_600_000);
    assert_eq!(oauth.logout().await.unwrap(), DeleteOutcome::Deleted);
    assert!(!fixture.credential_file().exists());
    assert_eq!(oauth.logout().await.unwrap(), DeleteOutcome::Missing);
}

#[tokio::test]
async fn preparation_refreshes_an_expired_session_and_persists_it() {
    let fresh = access_token("acct_test", "fresh");
    let server = FakeServer::start([token_reply(&fresh)]);
    let fixture = Fixture::new();
    fixture.write_session(&access_token("acct_test", "stale"), now_ms() - 1);
    let access = prepare_chatgpt_credential(&fixture.oauth(&server))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(access.account_id(), "acct_test");
    assert!(access.refresh_after_ms() > now_ms());
    assert_eq!(fixture.saved()["access_token"], fresh);
    assert_eq!(mode(&fixture.credential_file()), 0o600);
    let body = server.requests()[0].json();
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], REFRESH_TOKEN);
    assert_eq!(access.into_token(), fresh);
}

#[tokio::test]
async fn preparation_reports_missing_and_expired_logins_as_absent() {
    let server = FakeServer::start([Reply::status(
        401,
        r#"{"error":{"code":"refresh_token_expired"}}"#,
    )]);
    let fixture = Fixture::new();
    let oauth = fixture.oauth(&server);
    assert_eq!(prepare_chatgpt_credential(&oauth).await, Ok(None));
    fixture.write_session(&access_token("acct_test", "stale"), now_ms() - 1);
    assert_eq!(prepare_chatgpt_credential(&oauth).await, Ok(None));
    assert!(!fixture.credential_file().exists());
}

#[tokio::test]
async fn preparation_refuses_credentials_readable_by_others() {
    let server = FakeServer::start([]);
    let fixture = Fixture::new();
    fixture.write_session(&access_token("acct_test", "shared"), now_ms() + 3_600_000);
    std::fs::set_permissions(
        fixture.credential_file(),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(
        prepare_chatgpt_credential(&fixture.oauth(&server)).await,
        Err(PreparationError::CredentialStorageUnavailable)
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn forced_refreshes_must_keep_the_signed_in_account() {
    let server = FakeServer::start([token_reply(&access_token("acct_test", "rotated"))]);
    let fixture = Fixture::new();
    fixture.write_session(&access_token("acct_test", "current"), now_ms() + 3_600_000);
    let oauth = fixture.oauth(&server);
    assert_eq!(
        refresh_chatgpt_credential(&oauth, RefreshMode::Force, "acct_other")
            .await
            .unwrap_err(),
        ChatGptError::ChatGptAccountChanged
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn credentials_never_appear_in_debug_output_or_errors() {
    let secret = access_token("acct_test", "debug");
    let server = FakeServer::start([]);
    let fixture = Fixture::new();
    fixture.write_session(&secret, now_ms() + 3_600_000);
    let oauth = fixture.oauth(&server);
    let access = prepare_chatgpt_credential(&oauth).await.unwrap().unwrap();
    for rendered in [format!("{access:?}"), format!("{oauth:?}")] {
        assert!(!rendered.contains(&secret), "{rendered}");
        assert!(!rendered.contains(REFRESH_TOKEN), "{rendered}");
    }
    assert!(
        !ChatGptError::CredentialRefreshRejected
            .to_string()
            .contains(&secret)
    );
}
