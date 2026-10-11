use zeroize::Zeroizing;

use super::*;

fn key(source: GatewaySource, team: Option<&str>, refresh_after_ms: Option<i64>) -> GatewayKey {
    GatewayKey {
        source,
        secret: Zeroizing::new("secret-token".to_owned()),
        team_id: team.map(str::to_owned),
        team_slug: None,
        refresh_after_ms,
    }
}

fn public_reason(access: &CatalogAccess) -> Option<PublicOnlyReason> {
    match access {
        CatalogAccess::PublicOnly(reason) => Some(*reason),
        CatalogAccess::Authenticated { .. } | CatalogAccess::HostManaged => None,
    }
}

#[test]
fn a_missing_credential_lists_the_public_catalog() {
    assert_eq!(
        public_reason(&gateway_catalog_access(None, true, 0)),
        Some(PublicOnlyReason::NoCredential)
    );
}

#[test]
fn a_login_needs_a_fresh_token_and_a_valid_team() {
    let due = key(GatewaySource::FxLogin, Some("team_1"), Some(100));
    assert_eq!(
        public_reason(&gateway_catalog_access(Some(&due), false, 100)),
        Some(PublicOnlyReason::LoginRefreshRequired)
    );
    for team in [None, Some("team one")] {
        let teamless = key(GatewaySource::FxLogin, team, Some(i64::MAX));
        assert_eq!(
            public_reason(&gateway_catalog_access(Some(&teamless), false, 0)),
            Some(PublicOnlyReason::LoginTeamRequired),
            "{team:?}"
        );
    }
    let ready = key(GatewaySource::FxLogin, Some("team_1"), Some(i64::MAX));
    let CatalogAccess::Authenticated {
        source,
        credential,
        login_team,
        explicit,
    } = gateway_catalog_access(Some(&ready), true, 0)
    else {
        panic!("a signed-in team is authenticated");
    };
    assert_eq!(source, "fx_login");
    assert_eq!(credential.as_str(), "secret-token");
    assert_eq!(login_team.as_deref(), Some("team_1"));
    assert!(explicit);
}

#[test]
fn keys_authenticate_without_a_team_whatever_their_expiry() {
    for source in [
        GatewaySource::VercelOidcToken,
        GatewaySource::AiGatewayApiKey,
        GatewaySource::StoredKey,
    ] {
        let keyed = key(source, Some("team_1"), Some(0));
        let CatalogAccess::Authenticated {
            source: named,
            login_team,
            explicit,
            ..
        } = gateway_catalog_access(Some(&keyed), false, 10)
        else {
            panic!("{source:?} is authenticated");
        };
        assert_eq!(named, source.name());
        assert_eq!(login_team, None);
        assert!(!explicit);
    }
}
