use ofx_testkit::{FakeServer, Reply};

use super::*;
use crate::provider_activation::tests::{ACCESS_TOKEN, Fixture, catalog, release};

const CODEX_SETTINGS: &str = r#"{"provider":"codex","models":{"codex":"gpt-6.1-sol"}}"#;

struct Listed {
    listing: Listing,
    stdout: String,
    stderr: String,
}

async fn list(profile: &Profile, format: OutputFormat, host_managed: bool) -> Listed {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let listing = match codex_selection(profile, &mut stderr) {
        Ok(()) => list_models(profile, format, host_managed, &mut stdout, &mut stderr).await,
        Err(listing) => listing,
    };
    Listed {
        listing,
        stdout: String::from_utf8(stdout).unwrap(),
        stderr: String::from_utf8(stderr).unwrap(),
    }
}

struct Unwritable;

impl Write for Unwritable {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::StorageFull))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn signed_in_with_codex() -> Fixture {
    let fixture = Fixture::new();
    fixture.signed_in();
    fixture.write_settings(CODEX_SETTINGS);
    fixture
}

#[tokio::test]
async fn codex_models_are_listed_in_catalog_order_with_their_source() {
    let fixture = signed_in_with_codex();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([
        release(),
        catalog(&["gpt-6.1-sol", "gpt-5.6-terra", "gpt-5.6-luna"]),
        catalog(&["gpt-6.1-sol", "gpt-5.6-terra", "gpt-5.6-luna"]),
    ]);
    let profile = fixture.profile(&auth, &server);
    let text = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(text.listing, Listing::Listed);
    assert_eq!(
        text.stdout,
        "[models] 3 available\n - gpt-6.1-sol · Codex subscription\n - gpt-5.6-terra · Codex subscription\n - gpt-5.6-luna · Codex subscription\n"
    );
    assert_eq!(text.stderr, "");
    let json = list(&profile, OutputFormat::Json, false).await;
    assert_eq!(json.listing, Listing::Listed);
    assert_eq!(
        json.stdout,
        "{\"kind\":\"models\",\"count\":3,\"shown_count\":3,\"more_count\":0,\"private_models_hidden\":false,\"ids\":[\"gpt-6.1-sol\",\"gpt-5.6-terra\",\"gpt-5.6-luna\"],\"models\":[{\"id\":\"gpt-6.1-sol\",\"source\":\"Codex subscription\"},{\"id\":\"gpt-5.6-terra\",\"source\":\"Codex subscription\"},{\"id\":\"gpt-5.6-luna\",\"source\":\"Codex subscription\"}]}\n"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].header("authorization"),
        Some(format!("Bearer {ACCESS_TOKEN}").as_str())
    );
    assert_eq!(fixture.settings().as_deref(), Some(CODEX_SETTINGS));
}

#[tokio::test]
async fn an_empty_codex_catalog_names_the_subscription() {
    let fixture = signed_in_with_codex();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([release(), catalog(&[]), catalog(&[])]);
    let profile = fixture.profile(&auth, &server);
    let text = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(
        text.stdout,
        "[models] no models returned by Codex subscription\n"
    );
    let json = list(&profile, OutputFormat::Json, false).await;
    assert_eq!(
        json.stdout,
        "{\"kind\":\"models\",\"count\":0,\"shown_count\":0,\"more_count\":0,\"private_models_hidden\":false,\"ids\":[],\"models\":[]}\n"
    );
}

#[tokio::test]
async fn catalog_failures_are_reported_with_upstream_details_and_codes() {
    for (reply, detail, code) in [
        (
            Reply::status(401, "{}"),
            "AuthenticationRejected",
            "AuthenticationRejected",
        ),
        (Reply::status(429, "{}"), "Unavailable", "RateLimited"),
        (
            Reply::status(503, "{}"),
            "Unavailable",
            "GatewayUnavailable",
        ),
        (Reply::status(404, "{}"), "Unavailable", "Unavailable"),
        (
            Reply::status(200, "{}"),
            "MalformedResponse",
            "MalformedResponse",
        ),
    ] {
        let fixture = signed_in_with_codex();
        let auth = FakeServer::start([]);
        let server = FakeServer::start([release(), reply.clone(), reply]);
        let profile = fixture.profile(&auth, &server);
        let text = list(&profile, OutputFormat::Text, false).await;
        assert_eq!(text.listing, Listing::Failed);
        assert_eq!(text.stdout, "");
        assert_eq!(
            text.stderr,
            format!("oh-fx models: could not list models: {detail}\n")
        );
        let json = list(&profile, OutputFormat::Json, false).await;
        assert_eq!(json.listing, Listing::Failed);
        assert_eq!(json.stderr, "");
        assert_eq!(
            json.stdout,
            format!(
                "{{\"kind\":\"models\",\"error\":\"could not list models: {detail}\",\"code\":\"{code}\"}}\n"
            )
        );
    }
}

#[tokio::test]
async fn without_a_codex_login_the_catalog_is_rejected_without_a_request() {
    let fixture = Fixture::new();
    fixture.write_settings(CODEX_SETTINGS);
    let auth = FakeServer::start([]);
    let server = FakeServer::start([]);
    let text = list(&fixture.profile(&auth, &server), OutputFormat::Text, false).await;
    assert_eq!(text.listing, Listing::Failed);
    assert_eq!(
        text.stderr,
        "oh-fx models: could not list models: AuthenticationRejected\n"
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_json_failure_that_cannot_be_written_is_reported_on_stderr() {
    let fixture = Fixture::new();
    fixture.write_settings(CODEX_SETTINGS);
    let auth = FakeServer::start([]);
    let server = FakeServer::start([]);
    let mut stderr = Vec::new();
    let profile = fixture.profile(&auth, &server);
    assert_eq!(codex_selection(&profile, &mut stderr), Ok(()));
    let listing = list_models(
        &profile,
        OutputFormat::Json,
        false,
        &mut Unwritable,
        &mut stderr,
    )
    .await;
    assert_eq!(listing, Listing::Failed);
    assert_eq!(String::from_utf8(stderr).unwrap(), "oh-fx: WriteFailed\n");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn host_managed_catalogs_need_no_local_login() {
    let fixture = Fixture::new();
    fixture.write_settings(CODEX_SETTINGS);
    let auth = FakeServer::start([]);
    let server = FakeServer::start([catalog(&["gpt-6.1-sol"])]);
    let text = list(&fixture.profile(&auth, &server), OutputFormat::Text, true).await;
    assert_eq!(text.listing, Listing::Listed);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/backend-api/codex/models");
    assert_eq!(requests[0].header("authorization"), None);
}

#[tokio::test]
async fn other_providers_are_left_to_the_unavailable_command() {
    let fixture = signed_in_with_codex();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([]);
    let mut profile = fixture.profile(&auth, &server);
    profile.lookup = |name| (name == "OH_FX_PROVIDER").then(|| "gateway".to_owned());
    let listed = list(&profile, OutputFormat::Json, false).await;
    assert_eq!(listed.listing, Listing::NotCodex);
    assert_eq!(listed.stdout, "");
    assert_eq!(listed.stderr, "");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn codex_needs_a_model_before_its_catalog_is_listed() {
    let fixture = Fixture::new();
    fixture.signed_in();
    fixture.write_settings(r#"{"provider":"codex"}"#);
    let auth = FakeServer::start([]);
    let server = FakeServer::start([release(), catalog(&["gpt-6.1-sol"])]);
    let mut profile = fixture.profile(&auth, &server);
    let unselected = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(unselected.listing, Listing::Failed);
    assert_eq!(
        unselected.stderr,
        "oh-fx: no Codex model is selected; run `oh-fx provider codex` to choose one, or set a model for this run with --model or OH_FX_MODEL\n"
    );
    assert!(server.requests().is_empty());
    profile.lookup = |name| (name == "OH_FX_MODEL").then(|| "gpt-5.6-luna".to_owned());
    assert_eq!(
        list(&profile, OutputFormat::Text, false).await.listing,
        Listing::Listed
    );
}

#[tokio::test]
async fn unusable_settings_fail_before_the_catalog() {
    let auth = FakeServer::start([]);
    let server = FakeServer::start([]);
    let fixture = Fixture::new();
    fixture.write_settings("{\"provider\":");
    let broken = list(&fixture.profile(&auth, &server), OutputFormat::Json, false).await;
    assert_eq!(broken.listing, Listing::Failed);
    assert_eq!(broken.stderr, "oh-fx: InvalidProfileConfiguration\n");
    assert_eq!(broken.stdout, "");

    let fixture = signed_in_with_codex();
    let mut profile = fixture.profile(&auth, &server);
    profile.lookup = |name| (name == "OH_FX_PROVIDER").then(|| "not a provider".to_owned());
    let invalid = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(invalid.stderr, "oh-fx: InvalidProviderValue\n");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn settings_diagnostics_are_reported_before_the_listing() {
    let fixture = signed_in_with_codex();
    std::fs::write(
        fixture.workspace.join(".oh-fx.json"),
        r#"{"provider":"gateway"}"#,
    )
    .unwrap();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([release(), catalog(&["gpt-6.1-sol"])]);
    let listed = list(&fixture.profile(&auth, &server), OutputFormat::Text, false).await;
    assert_eq!(listed.listing, Listing::Listed);
    assert_eq!(
        listed.stderr,
        "oh-fx: config project: ignored_project_user_only_setting; key=provider\n"
    );
}

#[test]
fn settings_are_answered_before_any_async_runtime_starts() {
    let auth = FakeServer::start([]);
    let server = FakeServer::start([]);
    let selection = |profile: &Profile| {
        let mut stderr = Vec::new();
        let selected = codex_selection(profile, &mut stderr);
        (selected, String::from_utf8(stderr).unwrap())
    };
    let fixture = signed_in_with_codex();
    let mut profile = fixture.profile(&auth, &server);
    assert_eq!(selection(&profile), (Ok(()), String::new()));
    profile.lookup = |name| (name == "OH_FX_PROVIDER").then(|| "gateway".to_owned());
    assert_eq!(selection(&profile), (Err(Listing::NotCodex), String::new()));
    let broken = Fixture::new();
    broken.write_settings("{\"provider\":");
    assert_eq!(
        selection(&broken.profile(&auth, &server)),
        (
            Err(Listing::Failed),
            "oh-fx: InvalidProfileConfiguration\n".to_owned()
        )
    );
    let unselected = Fixture::new();
    unselected.write_settings(r#"{"provider":"codex"}"#);
    assert_eq!(
        selection(&unselected.profile(&auth, &server)).0,
        Err(Listing::Failed)
    );
    assert!(server.requests().is_empty());
}

#[test]
fn model_ids_are_shown_terminal_safe_in_text_and_raw_in_json() {
    let ids = vec!["gpt-6.1-sol\u{9b}31m".to_owned(), "a\u{202e}b".to_owned()];
    let text = text_listing(&ids);
    assert!(!text.contains('\u{9b}'), "{text:?}");
    assert!(!text.contains('\u{202e}'), "{text:?}");
    assert!(text.starts_with("[models] 2 available\n - gpt-6.1-sol"));
    assert!(json_listing(&ids).contains("gpt-6.1-sol\u{9b}31m"));
}
