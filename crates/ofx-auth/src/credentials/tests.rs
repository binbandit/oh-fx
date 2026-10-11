use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::*;
use crate::login_flow::VercelEndpoints;

pub(super) fn write_session(directory: &Path, issuer: &str, expires_at_ms: i64, team: &str) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = directory.join("auth.json");
    std::fs::write(
        &file,
        format!(
            "{{\"version\":1,\"issuer\":\"{issuer}\",\"client_id\":\"client\",\"access_token\":\"login-access\",\"refresh_token\":\"login-refresh\",\"expires_at_ms\":{expires_at_ms},\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\"{team}}}"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
}

pub(super) fn login(directory: &Path) -> VercelOAuth {
    VercelOAuth::new(
        directory.to_owned(),
        "oh-fx/test",
        VercelEndpoints::default(),
    )
}

const TEAM: &str = ",\"team_slug\":\"team-slug\",\"team_id\":\"team_id\"";

struct Fixture {
    _root: tempfile::TempDir,
    data: std::path::PathBuf,
    store: StoredKeyStore,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("oh-fx");
        let store = StoredKeyStore::in_profile(Some(&data));
        Self {
            _root: root,
            data,
            store,
        }
    }

    fn save_key(&self) {
        self.store
            .save(|| Some(Zeroizing::new("vck-stored".to_owned())))
            .unwrap();
    }

    fn resolution(
        &self,
        variables: &[(&str, &str)],
        preferred: Option<GatewaySource>,
    ) -> GatewayResolution {
        let lookup = |name: &str| {
            variables
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| (*value).to_owned())
        };
        let login = login(&self.data);
        GatewaySources {
            lookup: &lookup,
            store: &self.store,
            login: Some(&login),
            preferred,
        }
        .stored()
    }

    fn resolved(&self, variables: &[(&str, &str)]) -> Option<(GatewaySource, String)> {
        self.resolution(variables, None)
            .key
            .map(|key| (key.source, key.secret.to_string()))
    }
}

#[test]
fn auth_mode_accepts_only_local_and_host_managed_process_values() {
    for valid in [None, Some(&b"local"[..]), Some(b"host-managed")] {
        assert!(is_valid_auth_mode(valid), "{valid:?}");
    }
    for invalid in [&b""[..], b"Local", b" local", b"host_managed", b"remote"] {
        assert!(!is_valid_auth_mode(Some(invalid)), "{invalid:?}");
    }
    assert_eq!(parse_auth_mode(None), Some(AuthMode::Local));
    assert_eq!(
        parse_auth_mode(Some(b"host-managed")),
        Some(AuthMode::HostManaged)
    );
}

#[test]
fn missing_credential_messages_use_surface_commands_in_preferred_order() {
    for message in [MISSING_CHATGPT_CREDENTIAL_MESSAGE, CHATGPT_RELOGIN_MESSAGE] {
        assert!(message.contains("Run oh-fx login codex"), "{message}");
    }
    assert!(MISSING_GATEWAY_CREDENTIAL_MESSAGE.starts_with("oh-fx needs"));
    assert!(UNREADABLE_STORED_KEY_MESSAGE.starts_with("oh-fx could"));
    let position = |text| MISSING_GATEWAY_CREDENTIAL_MESSAGE.find(text).unwrap();
    assert!(position("oh-fx login") < position("oh-fx setup"));
    assert!(position("oh-fx setup") < position("AI_GATEWAY_API_KEY"));
    assert!(UNREADABLE_STORED_KEY_MESSAGE.contains(crate::stored_key::STORED_KEY_BACKEND));
    assert!(SELECTED_STORED_KEY_UNREADABLE.contains(crate::stored_key::STORED_KEY_BACKEND));
}

#[test]
fn source_labels_and_names_follow_upstream() {
    assert!(
        GatewaySource::StoredKey
            .label()
            .contains(crate::stored_key::STORED_KEY_BACKEND)
    );
    for (source, label, name) in [
        (
            GatewaySource::VercelOidcToken,
            "VERCEL_OIDC_TOKEN",
            "vercel_oidc_token",
        ),
        (
            GatewaySource::AiGatewayApiKey,
            "AI_GATEWAY_API_KEY",
            "ai_gateway_api_key",
        ),
        (GatewaySource::FxLogin, "oh-fx login", "fx_login"),
    ] {
        assert_eq!(source.label(), label);
        assert_eq!(source.name(), name);
        assert_eq!(GatewaySource::parse(name), Some(source));
        assert_ne!(source.label(), GatewaySource::StoredKey.label());
    }
    assert_eq!(GatewaySource::parse("keychain"), None);
    assert!(GatewaySource::FxLogin.refreshable());
    assert!(!GatewaySource::StoredKey.refreshable());
}

#[test]
fn the_oidc_token_outranks_the_api_key_and_blank_values_are_absent() {
    let fixture = Fixture::new();
    let oidc = GatewaySource::VercelOidcToken;
    let key = GatewaySource::AiGatewayApiKey;
    assert_eq!(fixture.resolved(&[]), None);
    assert_eq!(
        fixture.resolved(&[
            ("AI_GATEWAY_API_KEY", "key"),
            ("VERCEL_OIDC_TOKEN", "token")
        ]),
        Some((oidc, "token".to_owned()))
    );
    assert_eq!(
        fixture.resolved(&[
            ("AI_GATEWAY_API_KEY", " key\n"),
            ("VERCEL_OIDC_TOKEN", " \t\r\n")
        ]),
        Some((key, " key\n".to_owned()))
    );
    assert_eq!(fixture.resolved(&[("AI_GATEWAY_API_KEY", "")]), None);
    assert_eq!(
        fixture.resolution(&[], None).help(),
        Some(MISSING_GATEWAY_CREDENTIAL_MESSAGE)
    );
}

#[test]
fn the_oh_fx_login_answers_after_the_environment_and_before_the_stored_key() {
    let fixture = Fixture::new();
    fixture.save_key();
    assert_eq!(
        fixture.resolved(&[]),
        Some((GatewaySource::StoredKey, "vck-stored".to_owned()))
    );
    write_session(&fixture.data, "https://vercel.com", i64::MAX, TEAM);
    let resolution = fixture.resolution(&[], None);
    let key = resolution.key().unwrap();
    assert_eq!(key.source, GatewaySource::FxLogin);
    assert_eq!(key.secret.as_str(), "login-access");
    assert_eq!(key.gateway_team(), Some("team_id"));
    assert_eq!(key.display_team(), Some("team-slug"));
    assert_eq!(resolution.help(), None);
    assert_eq!(
        fixture.resolved(&[("AI_GATEWAY_API_KEY", "vck-env")]),
        Some((GatewaySource::AiGatewayApiKey, "vck-env".to_owned()))
    );
}

#[test]
fn the_stored_key_answers_only_after_the_environment_sources() {
    let fixture = Fixture::new();
    fixture.save_key();
    std::fs::write(fixture.data.join("api-key"), "k".repeat(9 * 1024)).unwrap();
    let unreadable = fixture.resolution(&[], None);
    assert!(unreadable.key().is_none());
    assert!(unreadable.unreadable());
    assert_eq!(unreadable.help(), Some(UNREADABLE_STORED_KEY_MESSAGE));
    assert_eq!(
        fixture.resolved(&[("VERCEL_OIDC_TOKEN", "oidc")]),
        Some((GatewaySource::VercelOidcToken, "oidc".to_owned()))
    );
}

#[test]
fn an_unusable_login_falls_through_to_the_stored_key() {
    let fixture = Fixture::new();
    fixture.save_key();
    write_session(&fixture.data, "https://example.com", i64::MAX, TEAM);
    let resolution = fixture.resolution(&[], None);
    assert_eq!(resolution.key().unwrap().source, GatewaySource::StoredKey);
    assert_eq!(resolution.login, Read::Unavailable);
    std::fs::remove_file(fixture.data.join("api-key")).unwrap();
    let resolution = fixture.resolution(&[], None);
    assert!(resolution.key().is_none());
    assert_eq!(
        resolution.help(),
        Some(PreparationError::CredentialStorageUnavailable.notice())
    );
    assert_eq!(
        resolution.prepared().err(),
        Some(GatewayRefusal::Preparation(
            PreparationError::CredentialStorageUnavailable
        ))
    );
}

#[test]
fn a_disabled_store_still_reports_why_the_login_was_silent() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    write_session(&data, "https://example.com", i64::MAX, TEAM);
    let lookup = |name: &str| (name == "OH_FX_DISABLE_KEYCHAIN").then(|| "1".to_owned());
    let store = StoredKeyStore::from_environment(Some(&data), &lookup);
    let login = login(&data);
    let resolution = GatewaySources {
        lookup: &lookup,
        store: &store,
        login: Some(&login),
        preferred: None,
    }
    .stored();
    assert_eq!(resolution.login, Read::Unavailable);
    assert_eq!(resolution.stored_key, Read::NotAttempted);
}

#[test]
fn a_remembered_choice_outranks_the_environment_and_never_crosses_to_precedence() {
    let fixture = Fixture::new();
    fixture.save_key();
    let environment = [
        ("AI_GATEWAY_API_KEY", "vck-env"),
        ("VERCEL_OIDC_TOKEN", "oidc"),
    ];
    let chosen = fixture.resolution(&environment, Some(GatewaySource::AiGatewayApiKey));
    assert_eq!(chosen.key().unwrap().source, GatewaySource::AiGatewayApiKey);
    let stored = fixture.resolution(&environment, Some(GatewaySource::StoredKey));
    assert_eq!(stored.key().unwrap().secret.as_str(), "vck-stored");
    let missing = fixture.resolution(&environment, Some(GatewaySource::FxLogin));
    assert!(missing.key().is_none());
    assert_eq!(
        missing.help(),
        Some(
            "oh-fx login is selected but unavailable. Run oh-fx login to reconnect; no other credential was selected."
        )
    );
    write_session(&fixture.data, "https://example.com", 1, TEAM);
    let broken = fixture.resolution(&environment, Some(GatewaySource::FxLogin));
    assert_eq!(
        broken.help(),
        Some(PreparationError::CredentialStorageUnavailable.notice())
    );
    let unset = fixture.resolution(&[], Some(GatewaySource::VercelOidcToken));
    assert_eq!(
        unset.help(),
        Some(
            "VERCEL_OIDC_TOKEN is selected but unavailable. Set VERCEL_OIDC_TOKEN before starting oh-fx; no other credential was selected."
        )
    );
    std::fs::write(fixture.data.join("api-key"), "k".repeat(9 * 1024)).unwrap();
    assert_eq!(
        fixture
            .resolution(&[], Some(GatewaySource::StoredKey))
            .help(),
        Some(SELECTED_STORED_KEY_UNREADABLE)
    );
}

#[test]
fn a_remembered_login_is_read_without_refreshing_in_stored_mode() {
    let fixture = Fixture::new();
    write_session(&fixture.data, "https://vercel.com", 1, TEAM);
    let resolution = fixture.resolution(&[], Some(GatewaySource::FxLogin));
    let key = resolution.key().unwrap();
    assert_eq!(key.source, GatewaySource::FxLogin);
    assert!(key.needs_refresh_at(0));
}

#[test]
fn preparation_admits_only_a_fresh_login_with_a_valid_team() {
    let fixture = Fixture::new();
    write_session(&fixture.data, "https://vercel.com", i64::MAX, TEAM);
    let prepared = fixture.resolution(&[], None).prepared().unwrap();
    assert_eq!(prepared.gateway_team(), Some("team_id"));
    for team in ["", ",\"team_id\":\"bad team\""] {
        write_session(&fixture.data, "https://vercel.com", i64::MAX, team);
        assert_eq!(
            fixture.resolution(&[], None).prepared().err(),
            Some(GatewayRefusal::Missing(MISSING_GATEWAY_CREDENTIAL_MESSAGE))
        );
    }
    write_session(&fixture.data, "https://vercel.com", 100_000, TEAM);
    assert_eq!(
        fixture
            .resolution(&[], Some(GatewaySource::FxLogin))
            .prepared()
            .err(),
        Some(GatewayRefusal::Missing(
            "oh-fx login is selected but unavailable. Run oh-fx login to reconnect; no other credential was selected."
        ))
    );
}

#[test]
fn gateway_teams_allow_only_header_safe_identifiers() {
    assert!(valid_gateway_team("team_ABC-123"));
    for invalid in ["", "team id", "team\r\nx", "t\u{e9}am"] {
        assert!(!valid_gateway_team(invalid), "{invalid}");
    }
    assert!(valid_gateway_team(&"a".repeat(128)));
    assert!(!valid_gateway_team(&"a".repeat(129)));
}

#[test]
fn an_unreadable_stored_key_is_a_storage_failure_when_it_was_chosen() {
    let fixture = Fixture::new();
    fixture.save_key();
    std::fs::write(fixture.data.join("api-key"), "k".repeat(9 * 1024)).unwrap();
    assert_eq!(
        fixture.resolution(&[], None).prepared().err(),
        Some(GatewayRefusal::Missing(UNREADABLE_STORED_KEY_MESSAGE))
    );
    assert_eq!(
        fixture
            .resolution(&[], Some(GatewaySource::StoredKey))
            .prepared()
            .err(),
        Some(GatewayRefusal::Preparation(
            PreparationError::CredentialStorageUnavailable
        ))
    );
}

#[tokio::test]
async fn a_rejected_login_refresh_falls_through_to_the_stored_key() {
    let server = ofx_testkit::FakeServer::start_at_origin(|origin| {
        [
            ofx_testkit::Reply::status(
                200,
                format!(
                    r#"{{"issuer":"{origin}","device_authorization_endpoint":"{origin}/device","token_endpoint":"{origin}/token"}}"#
                ),
            ),
            ofx_testkit::Reply::status(400, r#"{"error":"invalid_grant"}"#),
        ]
    });
    let fixture = Fixture::new();
    fixture.save_key();
    write_session(
        &fixture.data,
        &format!("http://{}", server.address()),
        1,
        TEAM,
    );
    let lookup = |_: &str| None;
    let login = login(&fixture.data);
    let sources = GatewaySources {
        lookup: &lookup,
        store: &fixture.store,
        login: Some(&login),
        preferred: None,
    };
    let resolution = sources.refreshed().await;
    assert_eq!(resolution.login, Read::Unavailable);
    let key = resolution.prepared().unwrap();
    assert_eq!(key.source, GatewaySource::StoredKey);
    assert_eq!(key.secret.as_str(), "vck-stored");
    assert!(!fixture.data.join("auth.json").exists());
    let resolution = sources.refreshed().await;
    assert_eq!(resolution.key().unwrap().source, GatewaySource::StoredKey);
    assert_eq!(server.requests().len(), 2);
}
