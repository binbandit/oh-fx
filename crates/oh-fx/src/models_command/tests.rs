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
    let listing = match selection(profile, &mut stderr) {
        Ok(catalog) => {
            list_models(
                profile,
                catalog,
                format,
                host_managed,
                &mut stdout,
                &mut stderr,
            )
            .await
        }
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
    assert_eq!(selection(&profile, &mut stderr), Ok(Catalog::Codex));
    let listing = list_models(
        &profile,
        Catalog::Codex,
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
async fn configured_providers_are_left_to_the_unavailable_command() {
    let fixture = Fixture::new();
    fixture.write_settings(
        r#"{"provider":"local","models":{"local":"m"},"providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://127.0.0.1:9/v1","auth":{"type":"none"},"models":["m"]}}}"#,
    );
    let auth = FakeServer::start([]);
    let server = FakeServer::start([]);
    let listed = list(&fixture.profile(&auth, &server), OutputFormat::Json, false).await;
    assert_eq!(listed.listing, Listing::Unsupported);
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
    let selected = |profile: &Profile| {
        let mut stderr = Vec::new();
        let selected = selection(profile, &mut stderr);
        (selected, String::from_utf8(stderr).unwrap())
    };
    let fixture = signed_in_with_codex();
    let mut profile = fixture.profile(&auth, &server);
    assert_eq!(selected(&profile), (Ok(Catalog::Codex), String::new()));
    profile.lookup = |name| (name == "OH_FX_PROVIDER").then(|| "gateway".to_owned());
    assert_eq!(
        selected(&profile),
        (Ok(Catalog::Gateway(None)), String::new())
    );
    let broken = Fixture::new();
    broken.write_settings("{\"provider\":");
    assert_eq!(
        selected(&broken.profile(&auth, &server)),
        (
            Err(Listing::Failed),
            "oh-fx: InvalidProfileConfiguration\n".to_owned()
        )
    );
    let unselected = Fixture::new();
    unselected.write_settings(r#"{"provider":"codex"}"#);
    assert_eq!(
        selected(&unselected.profile(&auth, &server)).0,
        Err(Listing::Failed)
    );
    assert!(server.requests().is_empty());
}

#[test]
fn model_ids_are_shown_terminal_safe_in_text_and_raw_in_json() {
    let models = Models {
        ids: vec!["gpt-6.1-sol\u{9b}31m".to_owned(), "a\u{202e}b".to_owned()],
        source: Some(CHATGPT_SOURCE_LABEL),
        provider: CHATGPT_SOURCE_LABEL,
        public_only: None,
    };
    let text = text_listing(&models);
    assert!(!text.contains('\u{9b}'), "{text:?}");
    assert!(!text.contains('\u{202e}'), "{text:?}");
    assert!(text.starts_with("[models] 2 available\n - gpt-6.1-sol"));
    assert!(json_listing(&models).contains("gpt-6.1-sol\u{9b}31m"));
}

const GATEWAY_KEY: &str = "vck_live_0123456789abcdef";
const PUBLIC_NOTE: &str = "[models] Using the public model catalog; sign in with Vercel or use an AI Gateway API key for team-private models.\n";

fn gateway_listing(ids: &[&str]) -> Reply {
    let entries: Vec<String> = ids
        .iter()
        .map(|id| format!(r#"{{"id":"{id}","type":"language","tags":["tool-use"]}}"#))
        .collect();
    Reply::status(200, format!(r#"{{"data":[{}]}}"#, entries.join(",")))
}

fn gateway_profile(
    fixture: &Fixture,
    auth: &FakeServer,
    server: &FakeServer,
    keyed: bool,
) -> Profile {
    fixture.write_settings(r#"{"provider":"gateway"}"#);
    let mut profile = fixture.profile(auth, server);
    if keyed {
        profile.lookup = |name| (name == "AI_GATEWAY_API_KEY").then(|| GATEWAY_KEY.to_owned());
    }
    profile
}

#[tokio::test]
async fn gateway_models_are_listed_in_catalog_order_with_the_key() {
    let fixture = Fixture::new();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([
        gateway_listing(&["vendor/plain-1", "openai/gpt-5.6-sol"]),
        gateway_listing(&["vendor/plain-1", "openai/gpt-5.6-sol"]),
    ]);
    let profile = gateway_profile(&fixture, &auth, &server, true);
    let text = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(text.listing, Listing::Listed);
    assert_eq!(
        text.stdout,
        "[models] 2 available\n - openai/gpt-5.6-sol\n - vendor/plain-1\n"
    );
    assert_eq!(text.stderr, "");
    let json = list(&profile, OutputFormat::Json, false).await;
    assert_eq!(
        json.stdout,
        "{\"kind\":\"models\",\"count\":2,\"shown_count\":2,\"more_count\":0,\"private_models_hidden\":false,\"ids\":[\"openai/gpt-5.6-sol\",\"vendor/plain-1\"]}\n"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/v1/coding-agent/v1/models");
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {GATEWAY_KEY}").as_str())
    );
    assert!(auth.requests().is_empty());
}

#[tokio::test]
async fn without_a_credential_the_public_gateway_catalog_is_listed_and_explained() {
    let fixture = Fixture::new();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([
        gateway_listing(&["openai/gpt-5.6-sol"]),
        gateway_listing(&["openai/gpt-5.6-sol"]),
        gateway_listing(&[]),
    ]);
    let profile = gateway_profile(&fixture, &auth, &server, false);
    let text = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(
        text.stdout,
        format!("[models] 1 available\n - openai/gpt-5.6-sol\n{PUBLIC_NOTE}")
    );
    let json = list(&profile, OutputFormat::Json, false).await;
    assert!(
        json.stdout.contains("\"private_models_hidden\":true"),
        "{}",
        json.stdout
    );
    let empty = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(
        empty.stdout,
        format!("[models] no models returned by gateway\n{PUBLIC_NOTE}")
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.header("authorization").is_none())
    );
}

#[tokio::test]
async fn a_rejected_gateway_key_falls_back_to_the_public_catalog() {
    let fixture = Fixture::new();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([
        Reply::status(401, "{}"),
        gateway_listing(&["openai/gpt-5.6-sol"]),
    ]);
    let profile = gateway_profile(&fixture, &auth, &server, true);
    let text = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(
        text.stdout,
        "[models] 1 available\n - openai/gpt-5.6-sol\n[models] Your Gateway credential was rejected; using the public model catalog.\n"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].header("authorization"), None);
}

#[tokio::test]
async fn gateway_catalog_failures_use_the_upstream_details_and_codes() {
    for (reply, detail, code) in [
        (
            Reply::status(503, "{}"),
            "Unavailable",
            "GatewayUnavailable",
        ),
        (
            Reply::status(200, "{}"),
            "MalformedResponse",
            "MalformedResponse",
        ),
    ] {
        let fixture = Fixture::new();
        let auth = FakeServer::start([]);
        let server = FakeServer::start([reply.clone(), reply]);
        let profile = gateway_profile(&fixture, &auth, &server, true);
        let text = list(&profile, OutputFormat::Text, false).await;
        assert_eq!(text.listing, Listing::Failed);
        assert_eq!(
            text.stderr,
            format!("oh-fx models: could not list models: {detail}\n")
        );
        let json = list(&profile, OutputFormat::Json, false).await;
        assert_eq!(
            json.stdout,
            format!(
                "{{\"kind\":\"models\",\"error\":\"could not list models: {detail}\",\"code\":\"{code}\"}}\n"
            )
        );
    }
}

#[tokio::test]
async fn a_host_managed_gateway_catalog_sends_no_credential_and_needs_no_explanation() {
    let fixture = Fixture::new();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([gateway_listing(&["openai/gpt-5.6-sol"])]);
    let profile = gateway_profile(&fixture, &auth, &server, true);
    let text = list(&profile, OutputFormat::Text, true).await;
    assert_eq!(text.stdout, "[models] 1 available\n - openai/gpt-5.6-sol\n");
    assert_eq!(server.requests()[0].header("authorization"), None);
}

fn write_login(fixture: &Fixture, team: &str) {
    use std::os::unix::fs::PermissionsExt;
    let directory = &fixture.paths.data;
    std::fs::create_dir_all(directory).unwrap();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = directory.join("auth.json");
    std::fs::write(
        &file,
        format!(
            "{{\"version\":1,\"issuer\":\"https://vercel.com\",\"client_id\":\"client\",\"access_token\":\"{GATEWAY_KEY}\",\"refresh_token\":\"login-refresh\",\"expires_at_ms\":{},\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\"{team}}}",
            i64::MAX
        ),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn a_vercel_sign_in_lists_its_team_catalog_or_explains_a_missing_team() {
    let fixture = Fixture::new();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([
        gateway_listing(&["openai/gpt-5.6-sol"]),
        gateway_listing(&["openai/gpt-5.6-sol"]),
    ]);
    let profile = gateway_profile(&fixture, &auth, &server, false);
    write_login(&fixture, ",\"team_slug\":\"one\",\"team_id\":\"team_1\"");
    let team = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(team.stdout, "[models] 1 available\n - openai/gpt-5.6-sol\n");
    write_login(&fixture, "");
    let teamless = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(
        teamless.stdout,
        "[models] 1 available\n - openai/gpt-5.6-sol\n[models] Choose a Vercel team to load its private models.\n"
    );
    let requests = server.requests();
    assert_eq!(requests[0].path, "/v1/coding-agent/v1/models?teamId=team_1");
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {GATEWAY_KEY}").as_str())
    );
    assert_eq!(requests[0].header("x-vercel-ai-gateway-team"), None);
    assert_eq!(requests[1].path, "/v1/coding-agent/v1/models");
    assert_eq!(requests[1].header("authorization"), None);
}

#[tokio::test]
async fn a_chosen_credential_source_never_falls_back_to_the_public_catalog() {
    let fixture = Fixture::new();
    let auth = FakeServer::start([]);
    let server = FakeServer::start([
        Reply::status(401, "{}"),
        gateway_listing(&["openai/gpt-5.6-sol"]),
    ]);
    let mut profile = gateway_profile(&fixture, &auth, &server, true);
    fixture.write_settings(r#"{"provider":"gateway","credential_source":"ai_gateway_api_key"}"#);
    profile.lookup = |name| (name == "AI_GATEWAY_API_KEY").then(|| GATEWAY_KEY.to_owned());
    let text = list(&profile, OutputFormat::Text, false).await;
    assert_eq!(text.listing, Listing::Failed);
    assert_eq!(
        text.stderr,
        "oh-fx models: could not list models: AuthenticationRejected\n"
    );
    assert_eq!(server.requests().len(), 1);
}
