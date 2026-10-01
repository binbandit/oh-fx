use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;

use ofx_testkit::FakeServer;

use super::*;
use crate::login_command::tests::{Forward, approve, read_until_waiting, token_reply};
use crate::provider_activation::tests::{Fixture, catalog, release};

const SELECTED: &str = r#"{"provider":"codex","models":{"codex":"gpt-5.6-luna"}}"#;

async fn select(profile: &Profile, host_managed: bool) -> Result<String, ActivationFailure> {
    select_codex(profile, &mut Vec::new(), false, host_managed).await
}

#[tokio::test]
async fn codex_becomes_the_provider_with_a_saved_login() {
    let fixture = Fixture::new();
    fixture.signed_in();
    fixture.write_settings(r#"{"provider":"gateway","models":{"codex":"gpt-5.6-luna"}}"#);
    let auth = FakeServer::start([]);
    let server = FakeServer::start([release(), catalog(&["gpt-6.1-sol", "gpt-5.6-luna"])]);
    assert_eq!(
        select(&fixture.profile(&auth, &server), false).await,
        Ok("Provider set to Codex.\n".to_owned())
    );
    assert_eq!(
        fixture.settings().as_deref(),
        Some("{\"provider\":\"codex\",\"models\":{\"codex\":\"gpt-5.6-luna\"}}\n")
    );
}

#[tokio::test]
async fn a_selected_provider_with_a_model_and_login_is_left_alone() {
    let fixture = Fixture::new();
    fixture.signed_in();
    fixture.write_settings(SELECTED);
    let auth = FakeServer::start([]);
    let server = FakeServer::start([]);
    let mut profile = fixture.profile(&auth, &server);
    assert_eq!(
        select(&profile, false).await,
        Ok("Codex is already selected.\n".to_owned())
    );
    fixture.write_settings(r#"{"models":{"codex":"gpt-5.6-luna"}}"#);
    profile.lookup = |name| (name == "OH_FX_PROVIDER").then(|| "codex".to_owned());
    assert_eq!(
        select(&profile, false).await,
        Ok("Codex is already selected.\n".to_owned())
    );
    assert!(server.requests().is_empty());
    assert_eq!(
        fixture.settings().as_deref(),
        Some(r#"{"models":{"codex":"gpt-5.6-luna"}}"#)
    );
}

#[tokio::test]
async fn a_selected_provider_without_a_model_still_chooses_one() {
    let fixture = Fixture::new();
    fixture.signed_in();
    fixture.write_settings(r#"{"provider":"codex"}"#);
    let auth = FakeServer::start([]);
    let server = FakeServer::start([release(), catalog(&["gpt-6.1-sol"])]);
    assert_eq!(
        select(&fixture.profile(&auth, &server), false).await,
        Ok("Provider set to Codex.\n".to_owned())
    );
    assert_eq!(
        fixture.settings().as_deref(),
        Some("{\"provider\":\"codex\",\"models\":{\"codex\":\"gpt-6.1-sol\"}}\n")
    );
}

#[tokio::test]
async fn without_a_login_the_provider_command_signs_in_first() {
    let fixture = Fixture::new();
    fixture.write_settings(SELECTED);
    let auth = FakeServer::start([token_reply()]);
    let server = FakeServer::start([release(), catalog(&["gpt-6.1-sol"])]);
    let profile = fixture.profile(&auth, &server);
    let (sender, receiver) = mpsc::channel();
    let browser = tokio::task::spawn_blocking(move || {
        let printed = read_until_waiting(&receiver);
        approve(&printed);
        printed
    });
    let selected = select_codex(&profile, &mut Forward(sender), false, false).await;
    let printed = browser.await.unwrap();
    assert!(printed.starts_with("Open this URL to sign in with Codex:\n"));
    assert_eq!(
        selected,
        Ok("Signed in with Codex.\nProvider set to Codex.\n".to_owned())
    );
    assert!(fixture.credential_file().is_file());
    assert_eq!(
        fixture.settings().as_deref(),
        Some("{\"provider\":\"codex\",\"models\":{\"codex\":\"gpt-6.1-sol\"}}\n")
    );
}

#[tokio::test]
async fn a_failed_sign_in_selects_nothing() {
    let fixture = Fixture::new();
    let data_root = fixture.paths.data.parent().unwrap().to_owned();
    fs::create_dir_all(&data_root).unwrap();
    fs::set_permissions(&data_root, fs::Permissions::from_mode(0o500)).unwrap();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([]);
    let selected = select(&fixture.profile(&auth, &server), false).await;
    fs::set_permissions(&data_root, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        selected,
        Err(ActivationFailure::Detail(
            "Codex subscription: Saved credential storage is unavailable. Check the saved credential, then retry."
                .to_owned()
        ))
    );
    assert!(server.requests().is_empty());
    assert_eq!(fixture.settings(), None);
}

#[tokio::test]
async fn host_managed_selection_needs_no_local_login() {
    let fixture = Fixture::new();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([catalog(&["gpt-6.1-sol"])]);
    let profile = fixture.profile(&auth, &server);
    assert_eq!(
        select(&profile, true).await,
        Ok("Provider set to Codex.\n".to_owned())
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("authorization"), None);
    assert_eq!(
        select(&profile, true).await,
        Ok("Codex is already selected.\n".to_owned())
    );
    assert!(auth.requests().is_empty());
}
