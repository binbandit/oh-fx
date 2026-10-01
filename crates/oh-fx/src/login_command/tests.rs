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
