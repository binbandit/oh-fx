use std::fs;
use std::io::Read;
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use ofx_testkit::{FakeServer, Reply};
use serde_json::json;

use super::*;
use crate::provider_activation::tests::{ACCESS_TOKEN, Fixture, catalog, release};

pub(crate) struct Forward(pub(crate) Sender<Vec<u8>>);

impl Write for Forward {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.0.send(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn read_until_waiting(receiver: &Receiver<Vec<u8>>) -> String {
    let mut printed = Vec::new();
    while !String::from_utf8_lossy(&printed).ends_with("Waiting for browser authorization...\n") {
        printed.extend(receiver.recv_timeout(Duration::from_secs(10)).unwrap());
    }
    String::from_utf8(printed).unwrap()
}

fn query<'a>(url: &'a str, key: &str) -> &'a str {
    let start = url.find(&format!("&{key}=")).unwrap() + key.len() + 2;
    let rest = &url[start..];
    &rest[..rest.find('&').unwrap_or(rest.len())]
}

pub(crate) fn approve(printed: &str) {
    let url = printed.lines().nth(1).unwrap();
    let state = query(url, "state").to_owned();
    let redirect = query(url, "redirect_uri");
    let port: u16 = redirect
        .trim_start_matches("http%3A%2F%2F127.0.0.1%3A")
        .trim_end_matches("%2Fauth%2Fcallback")
        .parse()
        .unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(
            format!(
                "GET /auth/callback?code=auth-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
}

pub(crate) fn token_reply() -> Reply {
    Reply::status(
        200,
        json!({"access_token": ACCESS_TOKEN, "refresh_token": "rt-refresh-secret-0123456789", "expires_in": 3600})
            .to_string(),
    )
}

async fn sign_in(
    fixture: &Fixture,
    auth: &FakeServer,
    catalog_server: &FakeServer,
) -> Result<(), ActivationFailure> {
    let profile = fixture.profile(auth, catalog_server);
    let (sender, receiver) = mpsc::channel();
    let browser = tokio::task::spawn_blocking(move || approve(&read_until_waiting(&receiver)));
    let mut output = Forward(sender);
    let outcome = login_codex(&profile, &mut output, false).await;
    browser.await.unwrap();
    outcome
}

#[tokio::test]
async fn signing_in_with_codex_selects_codex_and_a_catalog_model() {
    let fixture = Fixture::new();
    let auth = FakeServer::start([token_reply()]);
    let catalog_server = FakeServer::start([release(), catalog(&["gpt-6.1-sol", "gpt-5.6-luna"])]);
    assert_eq!(sign_in(&fixture, &auth, &catalog_server).await, Ok(()));
    assert_eq!(
        fixture.settings().as_deref(),
        Some("{\"models\":{\"codex\":\"gpt-6.1-sol\"},\"provider\":\"codex\"}\n")
    );
    assert!(fixture.credential_file().is_file());
    assert_eq!(auth.requests().len(), 1);
    assert_eq!(catalog_server.requests().len(), 2);
}

#[tokio::test]
async fn a_failed_catalog_keeps_the_sign_in_but_selects_nothing() {
    let fixture = Fixture::new();
    let auth = FakeServer::start([token_reply()]);
    let catalog_server = FakeServer::start([release(), Reply::status(503, "")]);
    assert_eq!(
        sign_in(&fixture, &auth, &catalog_server).await,
        Err(ActivationFailure::Detail(
            "could not load the target model catalog (gateway_unavailable)".to_owned()
        ))
    );
    assert!(fixture.credential_file().is_file());
    assert_eq!(fixture.settings(), None);
}

#[tokio::test]
async fn a_failed_sign_in_reports_the_login_failure_before_activation() {
    let fixture = Fixture::new();
    let data_root = fixture.paths.data.parent().unwrap().to_owned();
    fs::create_dir_all(&data_root).unwrap();
    fs::set_permissions(&data_root, fs::Permissions::from_mode(0o500)).unwrap();
    let auth = FakeServer::start([]);
    let catalog_server = FakeServer::start([]);
    let outcome = login_codex(
        &fixture.profile(&auth, &catalog_server),
        &mut Vec::new(),
        false,
    )
    .await;
    fs::set_permissions(&data_root, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        outcome,
        Err(ActivationFailure::Detail(
            "Codex subscription: Saved credential storage is unavailable. Check the saved credential, then retry."
                .to_owned()
        ))
    );
    assert!(catalog_server.requests().is_empty());
}

#[tokio::test]
async fn grok_sign_in_reaches_authenticated_userinfo_and_catalog_before_selection() {
    use crate::provider_activation::tests::grok_catalog;
    let fixture = Fixture::new();
    let auth=FakeServer::start([Reply::status(200,json!({"access_token":"grok-login-secret","refresh_token":"grok-refresh-secret","expires_in":3600}).to_string()),Reply::status(200,r#"{"sub":"grok-account"}"#)]);
    let catalog_server = FakeServer::start([
        Reply::status(200, "1.0.13\n"),
        grok_catalog(&["grok-current"]),
        Reply::status(503, ""),
    ]);
    let profile = fixture.grok_profile(&auth, &catalog_server);
    let (sender, receiver) = mpsc::channel();
    let browser = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        loop {
            bytes.extend(receiver.recv_timeout(Duration::from_secs(10)).unwrap());
            let printed = String::from_utf8_lossy(&bytes);
            if printed.contains("Waiting for browser authorization...\n") {
                break;
            }
        }
        let printed = String::from_utf8(bytes).unwrap();
        assert!(
            printed.starts_with("Open this URL to sign in with Grok:\n"),
            "{printed}"
        );
        let url = printed.lines().nth(1).unwrap();
        let state = query(url, "state");
        let redirect = query(url, "redirect_uri");
        let port: u16 = redirect
            .trim_start_matches("http%3A%2F%2F127.0.0.1%3A")
            .trim_end_matches("%2Fcallback")
            .parse()
            .unwrap();
        assert!(![8976, 8977].contains(&port));
        let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
        socket.write_all(format!("GET /callback?code=manual-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    });
    let mut output = Forward(sender);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        login_grok(&profile, &mut output, false),
    )
    .await
    .unwrap();
    browser.await.unwrap();
    assert_eq!(result, Ok(()));
    assert!(fixture.grok_credential_file().is_file());
    let settings: serde_json::Value = serde_json::from_str(&fixture.settings().unwrap()).unwrap();
    assert_eq!(settings["provider"], "grok");
    assert_eq!(settings["models"]["grok"], "grok-current");
    let requests = auth.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/v1/oauth/token");
    assert_eq!(requests[1].path, "/v1/userinfo");
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer grok-login-secret")
    );
}

#[tokio::test]
async fn grok_logout_reports_missing_and_revocation_failure_without_retaining_local_session() {
    for present in [false, true] {
        let fixture = Fixture::new();
        if present {
            fixture.grok_signed_in();
        }
        let auth = FakeServer::start(if present {
            vec![Reply::status(503, "")]
        } else {
            vec![]
        });
        let catalog = FakeServer::start([]);
        let mut output = Vec::new();
        let mut errors = Vec::new();
        assert_eq!(
            logout_grok(
                &fixture.grok_profile(&auth, &catalog),
                &mut output,
                &mut errors
            )
            .await,
            ExitCode::SUCCESS
        );
        assert_eq!(
            String::from_utf8(output).unwrap(),
            if present {
                "Signed out of Grok.\n"
            } else {
                "No Grok login session found.\n"
            }
        );
        assert_eq!(
            String::from_utf8(errors).unwrap(),
            if present {
                "oh-fx logout: local Grok session removed, but remote revocation could not be confirmed\n"
            } else {
                ""
            }
        );
        assert!(!fixture.grok_credential_file().exists());
        assert_eq!(auth.requests().len(), usize::from(present));
        assert!(catalog.requests().is_empty());
    }
}

#[tokio::test]
async fn grok_logout_reports_durable_storage_and_output_failures() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let fixture = Fixture::new();
    let auth = FakeServer::start([]);
    let catalog = FakeServer::start([]);
    let profile = fixture.grok_profile(&auth, &catalog);
    let mut errors = Vec::new();
    assert_eq!(
        logout_grok(&profile, &mut Broken, &mut errors).await,
        ExitCode::FAILURE
    );
    assert_eq!(errors, b"oh-fx: WriteFailed\n");
    let outside = fixture.paths.data.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
    let outside_session = outside.join("grok-auth.json");
    fs::write(&outside_session, "{}").unwrap();
    std::os::unix::fs::symlink(&outside, &fixture.paths.data).unwrap();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    assert_eq!(
        logout_grok(&profile, &mut output, &mut errors).await,
        ExitCode::FAILURE
    );
    assert!(output.is_empty());
    assert_eq!(
        errors,
        b"oh-fx logout: failed to durably remove saved Grok login\n"
    );
    assert_eq!(fs::read_to_string(outside_session).unwrap(), "{}");
    assert!(auth.requests().is_empty());
}
