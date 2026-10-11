use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::subscription_session::DeleteOutcome;

const SESSION_JSON: &str = "{\"version\":1,\"issuer\":\"https://vercel.com\",\"client_id\":\"client\",\"access_token\":\"access\",\"refresh_token\":\"refresh\",\"expires_at_ms\":1,\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\",\"team_slug\":\"team-slug\",\"team_id\":\"team-id\"}";

fn session() -> Session {
    Session {
        issuer: ISSUER.to_owned(),
        client_id: "client".to_owned(),
        access_token: Secret::new("access".to_owned()),
        refresh_token: Secret::new("refresh".to_owned()),
        expires_at_ms: 1234,
        scope: "openid offline_access".to_owned(),
        token_type: "Bearer".to_owned(),
        team_slug: Some("vercel-labs".to_owned()),
        team_id: Some("team_123".to_owned()),
    }
}

fn lookup<'a>(entries: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        entries
            .iter()
            .find(|(variable, _)| *variable == name)
            .map(|(_, value)| (*value).to_owned())
    }
}

#[test]
fn oauth_session_stringifies_and_parses_in_upstream_format() {
    let text = stringify(&session());
    assert_eq!(
        text.as_str(),
        "{\"version\":1,\"issuer\":\"https://vercel.com\",\"client_id\":\"client\",\"access_token\":\"access\",\"refresh_token\":\"refresh\",\"expires_at_ms\":1234,\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\",\"team_slug\":\"vercel-labs\",\"team_id\":\"team_123\"}\n"
    );
    assert_eq!(parse(text.as_bytes()), Ok(session()));
    let teamless = Session {
        team_slug: None,
        team_id: None,
        ..session()
    };
    assert!(stringify(&teamless).ends_with("\"token_type\":\"Bearer\"}\n"));
    let parsed = parse(SESSION_JSON.as_bytes()).unwrap();
    assert_eq!(parsed.team_id.as_deref(), Some("team-id"));
    assert_eq!(stringify(&parsed).as_str(), format!("{SESSION_JSON}\n"));
    assert!(!format!("{parsed:?}").contains("\"access\""));
}

#[test]
fn oauth_session_parse_rejects_other_shapes_and_issuers() {
    let invalid_issuer = SESSION_JSON.replace("https://vercel.com", "https://example.com");
    let version_two = SESSION_JSON.replace("\"version\":1", "\"version\":2");
    let float_version = SESSION_JSON.replace("\"version\":1", "\"version\":1.0");
    let empty_scope = SESSION_JSON.replace("openid offline_access", "");
    let numeric_team = SESSION_JSON.replace("\"team-id\"", "7");
    let duplicate = SESSION_JSON.replace("{\"version\":1", "{\"version\":1,\"version\":1");
    let path_issuer = SESSION_JSON.replace("https://vercel.com", "http://127.0.0.1:43123/path");
    for invalid in [
        "[]",
        "{\"version\":2}\n",
        &invalid_issuer,
        &version_two,
        &float_version,
        &empty_scope,
        &numeric_team,
        &duplicate,
        &path_issuer,
    ] {
        assert_eq!(
            parse(invalid.as_bytes()),
            Err(SessionError::InvalidSession),
            "{invalid}"
        );
    }
    let loopback = SESSION_JSON.replace("https://vercel.com", "http://127.0.0.1:43123");
    assert_eq!(
        parse(loopback.as_bytes()).unwrap().issuer,
        "http://127.0.0.1:43123"
    );
    let null_team = SESSION_JSON.replace("\"team-id\"", "null");
    assert_eq!(parse(null_team.as_bytes()).unwrap().team_id, None);
}

#[test]
fn oauth_session_treats_near_expiry_as_expired() {
    let session = Session {
        expires_at_ms: 100_000,
        ..session()
    };
    assert!(session.expired(50_000));
    assert!(!session.expired(1));
    assert!(session.expired(i64::MAX));
    assert_eq!(refresh_deadline_ms(i64::MIN), i64::MIN);
}

#[test]
fn oauth_e2e_issuer_override_accepts_loopback_origins_only() {
    assert_eq!(configured_issuer_url(&lookup(&[])), ISSUER);
    for (value, expected) in [
        ("http://127.0.0.1:43123", "http://127.0.0.1:43123"),
        ("http://127.0.0.1:43123/", "http://127.0.0.1:43123"),
        ("http://localhost:43123", "http://localhost:43123"),
        ("https://example.com", ISSUER),
        ("http://127.0.0.1:43123@evil.example", ISSUER),
        ("http://127.0.0.1:43123/oauth", ISSUER),
        ("http://127.0.0.1", ISSUER),
    ] {
        let entries = [(E2E_ISSUER_VARIABLE, value)];
        assert_eq!(
            configured_issuer_url(&lookup(&entries)),
            expected,
            "{value}"
        );
    }
    assert_eq!(
        validate_e2e_endpoint(
            "http://127.0.0.1:43123",
            "http://localhost:43123/oauth/token"
        ),
        Ok(())
    );
    assert_eq!(
        validate_e2e_endpoint("http://127.0.0.1:43123", "https://example.com/oauth/token"),
        Err(VercelError::InvalidE2EOAuthEndpoint)
    );
    assert_eq!(
        validate_e2e_endpoint(ISSUER, "https://vercel.com/oauth/token"),
        Ok(())
    );
}

#[test]
fn the_client_id_override_must_not_be_blank() {
    assert_eq!(configured_client_id(&lookup(&[])), DEFAULT_CLIENT_ID);
    assert_eq!(
        configured_client_id(&lookup(&[(CLIENT_ID_VARIABLE, " \t\n")])),
        DEFAULT_CLIENT_ID
    );
    assert_eq!(
        configured_client_id(&lookup(&[(CLIENT_ID_VARIABLE, " cl_custom ")])),
        " cl_custom "
    );
}

#[tokio::test]
async fn the_session_store_is_private_and_tells_invalid_from_absent() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("oh-fx");
    let store = SessionStore::new(directory.clone());
    assert_eq!(store.load_unlocked(), Ok(None));
    store.save_new_session(&session()).await.unwrap();
    let file = directory.join("auth.json");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(store.load_unlocked(), Ok(Some(session())));
    std::fs::write(&file, "{\"version\":2}\n").unwrap();
    assert_eq!(store.load_unlocked(), Err(VercelError::InvalidAuthSession));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(store.load_unlocked(), Err(VercelError::InsecureAuthFile));
    let mutation = store.begin_existing_mutation().await.unwrap().unwrap();
    assert_eq!(mutation.delete(), Ok(DeleteOutcome::Deleted));
    assert_eq!(mutation.delete(), Ok(DeleteOutcome::Missing));
}
