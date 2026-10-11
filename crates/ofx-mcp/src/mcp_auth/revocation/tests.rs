use std::sync::{Arc, Mutex};

use zeroize::Zeroizing;

use super::*;
use crate::test_support::{FakeServer, Reply};

fn http() -> reqwest::Client {
    ofx_http::build_connection_client(&ofx_http::ConnectionOptions {
        follow_redirects: false,
        ..ofx_http::ConnectionOptions::default()
    })
    .unwrap()
}

fn grant(endpoint: Option<String>) -> Credentials {
    Credentials {
        endpoint: "https://mcp.example/mcp".to_owned(),
        resource: "https://mcp.example/mcp".to_owned(),
        issuer: "https://issuer.example".to_owned(),
        client_id: "client id".to_owned(),
        client_secret: None,
        access_token: Zeroizing::new("stored-access".to_owned()),
        refresh_token: Some(Zeroizing::new("stored-refresh".to_owned())),
        scope: "tools".to_owned(),
        token_type: "Bearer".to_owned(),
        token_endpoint_auth_method: "none".to_owned(),
        expires_at_ms: i64::MAX,
        authorization_endpoint: "https://issuer.example/authorize".to_owned(),
        token_endpoint: "https://issuer.example/token".to_owned(),
        revocation_endpoint: endpoint,
    }
}

async fn revoking(statuses: &[u16]) -> FakeServer {
    let statuses = Arc::new(Mutex::new(statuses.to_vec()));
    FakeServer::start(move |_| {
        let mut statuses = statuses.lock().unwrap();
        let status = if statuses.is_empty() {
            200
        } else {
            statuses.remove(0)
        };
        Reply::status(status).header("Location", "https://elsewhere.example/")
    })
    .await
}

fn revoke_url(server: &FakeServer) -> String {
    server.url.replace("/mcp", "/revoke")
}

#[tokio::test]
async fn revocation_posts_the_refresh_then_the_access_token() {
    let server = revoking(&[]).await;
    assert_eq!(
        revoke_credentials(&http(), &grant(Some(revoke_url(&server)))).await,
        Ok(())
    );
    let requests = server.requests();
    let bodies: Vec<&str> = requests
        .iter()
        .map(|request| request.body.as_str())
        .collect();
    assert_eq!(
        bodies,
        [
            "token=stored-refresh&token_type_hint=refresh_token&client_id=client%20id",
            "token=stored-access&token_type_hint=access_token&client_id=client%20id",
        ]
    );
    for request in &requests {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/revoke");
        assert_eq!(
            request.header("content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(request.header("authorization"), None);
    }
}

#[tokio::test]
async fn client_secrets_go_in_the_basic_header_or_the_form() {
    let server = revoking(&[]).await;
    let mut basic = grant(Some(revoke_url(&server)));
    basic.refresh_token = None;
    basic.token_endpoint_auth_method = "client_secret_basic".to_owned();
    basic.client_secret = Some(Zeroizing::new("s3cret".to_owned()));
    assert_eq!(revoke_credentials(&http(), &basic).await, Ok(()));
    let mut posted = basic.clone();
    posted.token_endpoint_auth_method = "client_secret_post".to_owned();
    assert_eq!(revoke_credentials(&http(), &posted).await, Ok(()));
    let requests = server.requests();
    assert_eq!(
        requests[0].header("authorization"),
        Some("Basic Y2xpZW50JTIwaWQ6czNjcmV0")
    );
    assert_eq!(
        requests[0].body,
        "token=stored-access&token_type_hint=access_token"
    );
    assert_eq!(
        requests[1].body,
        "token=stored-access&token_type_hint=access_token&client_id=client%20id&client_secret=s3cret"
    );
    let mut missing = posted.clone();
    missing.client_secret = None;
    assert_eq!(
        revoke_credentials(&http(), &missing).await,
        Err(McpError::ClientSecretMissing)
    );
    let mut unknown = posted.clone();
    unknown.token_endpoint_auth_method = "private_key_jwt".to_owned();
    assert_eq!(
        revoke_credentials(&http(), &unknown).await,
        Err(McpError::UnsupportedTokenEndpointAuthenticationMethod)
    );
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn only_200_counts_as_revoked_and_both_tokens_are_tried() {
    for status in [204, 400, 503, 302] {
        let server = revoking(&[status]).await;
        assert_eq!(
            revoke_credentials(&http(), &grant(Some(revoke_url(&server)))).await,
            Err(McpError::TokenRevocationFailed),
            "{status}"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "{status}");
        assert!(requests.iter().all(|request| request.path == "/revoke"));
    }
    let server = revoking(&[200, 503]).await;
    assert_eq!(
        revoke_credentials(&http(), &grant(Some(revoke_url(&server)))).await,
        Err(McpError::TokenRevocationFailed)
    );
}

#[tokio::test]
async fn a_grant_without_a_refresh_token_revokes_only_the_access_token() {
    let server = revoking(&[]).await;
    let mut access_only = grant(Some(revoke_url(&server)));
    access_only.refresh_token = None;
    assert_eq!(revoke_credentials(&http(), &access_only).await, Ok(()));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .body
            .starts_with("token=stored-access&token_type_hint=access_token")
    );
}

#[tokio::test]
async fn a_missing_or_insecure_endpoint_sends_nothing() {
    assert_eq!(
        revoke_credentials(&http(), &grant(None)).await,
        Err(McpError::RevocationEndpointMissing)
    );
    for endpoint in [
        "http://example.test/revoke",
        "https://user@issuer.example/revoke",
        "https://issuer.example/revoke#fragment",
    ] {
        assert_eq!(
            revoke_credentials(&http(), &grant(Some(endpoint.to_owned()))).await,
            Err(McpError::InsecureMcpAuthEndpoint),
            "{endpoint}"
        );
    }
}

#[tokio::test]
async fn a_response_over_256_kib_fails() {
    let large = "x".repeat(256 * 1024 + 1);
    let server = FakeServer::start(move |_| Reply::status(200).body(&large)).await;
    assert_eq!(
        revoke_credentials(&http(), &grant(Some(revoke_url(&server)))).await,
        Err(McpError::McpAuthDocumentTooLarge)
    );
}
