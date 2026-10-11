use ofx_testkit::{FakeServer, Reply};

use super::*;

fn transport() -> Transport {
    Transport::new("oh-fx/test").unwrap()
}

fn metadata(server: &FakeServer) -> Metadata {
    let origin = format!("http://{}", server.address());
    Metadata {
        issuer: origin.clone(),
        device_authorization_endpoint: format!("{origin}/device"),
        token_endpoint: format!("{origin}/token"),
        revocation_endpoint: None,
    }
}

fn metadata_body(issuer: &str, origin: &str) -> String {
    format!(
        r#"{{"issuer":"{issuer}","device_authorization_endpoint":"{origin}/device","token_endpoint":"{origin}/token","revocation_endpoint":"{origin}/revoke"}}"#
    )
}

#[tokio::test]
async fn oauth_discovery_maps_protocol_input_through_the_transport() {
    let server =
        FakeServer::start_at_origin(|origin| [Reply::status(200, metadata_body(origin, origin))]);
    let issuer = format!("http://{}", server.address());
    let metadata = discover(&transport(), &issuer).await.unwrap();
    assert_eq!(metadata.token_endpoint, format!("{issuer}/token"));
    assert_eq!(
        metadata.revocation_endpoint,
        Some(format!("{issuer}/revoke"))
    );
    let request = &server.requests()[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/.well-known/openid-configuration");
}

#[tokio::test]
async fn oauth_discovery_rejects_metadata_for_another_issuer() {
    let server = FakeServer::start_at_origin(|origin| {
        [
            Reply::status(200, metadata_body("https://vercel.com", origin)),
            Reply::status(500, metadata_body(origin, origin)),
        ]
    });
    let issuer = format!("http://{}", server.address());
    assert_eq!(
        discover(&transport(), &issuer).await,
        Err(VercelError::InvalidIssuer)
    );
    assert_eq!(
        discover(&transport(), &issuer).await,
        Err(VercelError::OAuthRequestFailed)
    );
}

#[tokio::test]
async fn oauth_device_authorization_owns_form_mapping_while_transport_owns_execution() {
    let server = FakeServer::start([Reply::status(
        200,
        r#"{"device_code":"device","user_code":"CODE","verification_uri":"https://vercel.test/verify","expires_in":600,"interval":5}"#,
    )]);
    let device = request_device_authorization(&transport(), &metadata(&server), "client id")
        .await
        .unwrap();
    assert_eq!(device.device_code.expose(), "device");
    assert_eq!(device.display_url(), "https://vercel.test/verify");
    let request = &server.requests()[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/device");
    assert_eq!(
        request.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        request.body_text(),
        "client_id=client%20id&scope=openid%20offline_access"
    );
}

#[tokio::test]
async fn oauth_polling_preserves_pending_slow_down_and_success_responses() {
    let token = r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#;
    let server = FakeServer::start([
        Reply::status(400, r#"{"error":"authorization_pending"}"#),
        Reply::status(400, r#"{"error":"slow_down"}"#),
        Reply::status(200, token),
        Reply::status(400, r#"{"error":"access_denied"}"#),
    ]);
    let metadata = metadata(&server);
    let mut results = Vec::new();
    for _ in 0..4 {
        results.push(poll_device_token(&transport(), &metadata, "client", "device").await);
    }
    assert_eq!(results[0], Ok(PollResult::Pending));
    assert_eq!(results[1], Ok(PollResult::SlowDown));
    let Ok(PollResult::Success(token)) = &results[2] else {
        panic!("expected a granted token");
    };
    assert_eq!(token.access_token.expose(), "access");
    assert_eq!(token.refresh_token.as_ref().unwrap().expose(), "refresh");
    assert_eq!(results[3], Err(VercelError::AccessDenied));
    let request = &server.requests()[0];
    assert_eq!(request.path, "/token");
    assert_eq!(
        request.body_text(),
        "client_id=client&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=device"
    );
}

#[tokio::test]
async fn oauth_refresh_posts_the_refresh_grant() {
    let server = FakeServer::start([Reply::status(
        200,
        r#"{"access_token":"next","refresh_token":"rotated","expires_in":60,"token_type":"bearer"}"#,
    )]);
    let token = refresh_token(&transport(), &metadata(&server), "client", "refresh value")
        .await
        .unwrap();
    assert_eq!(token.access_token.expose(), "next");
    assert_eq!(token.scope, "");
    assert_eq!(
        server.requests()[0].body_text(),
        "client_id=client&grant_type=refresh_token&refresh_token=refresh%20value"
    );
}

#[test]
fn a_reduced_grant_names_the_scope_the_issuer_withheld() {
    assert_eq!(
        missing_granted_scope("openid offline_access", "openid offline_access"),
        None
    );
    assert_eq!(
        missing_granted_scope("openid", "openid email profile"),
        None
    );
    assert_eq!(
        missing_granted_scope("openid offline_access", "openid"),
        Some("offline_access")
    );
    assert_eq!(
        missing_granted_scope(
            "openid offline_access use:ai-gateway",
            "openid offline_access"
        ),
        Some("use:ai-gateway")
    );
    assert_eq!(missing_granted_scope("", "openid"), None);
}

#[test]
fn oauth_maps_provider_errors() {
    for (body, expected) in [
        (
            &br#"{"error":"authorization_pending"}"#[..],
            VercelError::AuthorizationPending,
        ),
        (br#"{"error":"slow_down"}"#, VercelError::SlowDown),
        (br#"{"error":"access_denied"}"#, VercelError::AccessDenied),
        (br#"{"error":"expired_token"}"#, VercelError::ExpiredToken),
        (
            br#"{"error":"invalid_request"}"#,
            VercelError::OAuthRequestFailed,
        ),
        (br#"{"error":42}"#, VercelError::OAuthRequestFailed),
        (br#"{"error":"invalid_client"}"#, VercelError::InvalidClient),
        (br#"{"error":"invalid_grant"}"#, VercelError::InvalidGrant),
        (b"42", VercelError::OAuthRequestFailed),
        (b"not json", VercelError::OAuthRequestFailed),
    ] {
        assert_eq!(
            map_oauth_http_error(body),
            expected,
            "{}",
            String::from_utf8_lossy(body)
        );
    }
}

#[test]
fn oauth_parses_token_sets_and_device_authorizations() {
    let token = parse_token_set(
        br#"{"access_token":"access","refresh_token":"refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
    )
    .unwrap();
    assert_eq!(token.access_token.expose(), "access");
    assert_eq!(token.refresh_token.unwrap().expose(), "refresh");
    assert_eq!(token.expires_in, 3600);
    assert_eq!(token.scope, "openid offline_access");
    for invalid in [
        &br#"{"access_token":"a","expires_in":1,"token_type":"mac"}"#[..],
        br#"{"access_token":"a","expires_in":1.5,"token_type":"Bearer"}"#,
        br#"{"access_token":"","expires_in":1,"token_type":"Bearer"}"#,
        br#"{"access_token":"a","expires_in":1,"token_type":"Bearer","scope":""}"#,
        br#"{"access_token":"a","expires_in":1,"token_type":"Bearer","refresh_token":7}"#,
    ] {
        assert_eq!(
            parse_token_set(invalid),
            Err(VercelError::InvalidOAuthResponse)
        );
    }
    let device = parse_device_authorization(
        br#"{"device_code":"device","user_code":"user","verification_uri":"https://vercel.com/verify","verification_uri_complete":"https://vercel.com/verify?code=user","expires_in":600}"#,
    )
    .unwrap();
    assert_eq!(device.interval, 5);
    assert_eq!(device.display_url(), "https://vercel.com/verify?code=user");
    let device = parse_device_authorization(
        br#"{"device_code":"d","user_code":"u","verification_uri":"https://v","expires_in":-1,"interval":"7"}"#,
    )
    .unwrap();
    assert_eq!((device.expires_in, device.interval), (-1, 5));
}

#[test]
fn oauth_parsers_reject_non_object_json() {
    assert_eq!(
        parse_metadata(b"[]"),
        Err(VercelError::InvalidOAuthResponse)
    );
    assert_eq!(
        parse_device_authorization(b"null"),
        Err(VercelError::InvalidOAuthResponse)
    );
    assert_eq!(
        parse_token_set(b"\"token\""),
        Err(VercelError::InvalidOAuthResponse)
    );
    assert_eq!(
        parse_token_set(br#"{"access_token":"a","access_token":"b"}"#),
        Err(VercelError::OAuthRequestFailed)
    );
}
