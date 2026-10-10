use super::*;
use crate::test_support::{FakeServer, RecordedRequest, Reply};

fn credentials(token_endpoint: &str, method: &str, secret: Option<&str>) -> Credentials {
    Credentials {
        endpoint: "https://mcp.example/a".to_owned(),
        resource: "https://mcp.example/".to_owned(),
        issuer: "https://issuer.example".to_owned(),
        client_id: "client id".to_owned(),
        client_secret: secret.map(|value| Zeroizing::new(value.to_owned())),
        access_token: Zeroizing::new("old-access".to_owned()),
        refresh_token: Some(Zeroizing::new("old-refresh".to_owned())),
        scope: "read".to_owned(),
        token_type: "Bearer".to_owned(),
        token_endpoint_auth_method: method.to_owned(),
        expires_at_ms: 1,
        authorization_endpoint: "https://issuer.example/authorize".to_owned(),
        token_endpoint: token_endpoint.to_owned(),
        revocation_endpoint: None,
    }
}

fn http() -> reqwest::Client {
    ofx_http::build_connection_client(&ofx_http::ConnectionOptions {
        follow_redirects: false,
        ..ofx_http::ConnectionOptions::default()
    })
    .unwrap()
}

#[derive(Clone)]
struct Answer {
    status: u16,
    content_type: Option<&'static str>,
    body: String,
}

fn json(status: u16, body: &str) -> Answer {
    Answer {
        status,
        content_type: Some("application/json; charset=utf-8"),
        body: body.to_owned(),
    }
}

fn plain(status: u16, content_type: Option<&'static str>, body: &str) -> Answer {
    Answer {
        status,
        content_type,
        body: body.to_owned(),
    }
}

async fn refresh_with(
    answer: Answer,
    method: &str,
    secret: Option<&str>,
) -> (Result<Credentials, McpError>, Vec<RecordedRequest>) {
    let server = FakeServer::start(move |_| {
        let reply = Reply::status(answer.status).body(&answer.body);
        match answer.content_type {
            Some(content_type) => reply.header("Content-Type", content_type),
            None => reply,
        }
    })
    .await;
    let endpoint = server.url.replace("/mcp", "/token");
    let result = refresh_credentials(&http(), &credentials(&endpoint, method, secret)).await;
    (result, server.requests())
}

#[tokio::test]
async fn a_refresh_posts_the_grant_and_replaces_only_what_the_server_returns() {
    let before = now_ms();
    let (refreshed, requests) = refresh_with(
        json(
            200,
            r#"{"access_token":"new-access","token_type":"bearer","expires_in":3600}"#,
        ),
        "none",
        None,
    )
    .await;
    let refreshed = refreshed.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/token");
    assert_eq!(
        requests[0].header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(requests[0].header("authorization"), None);
    assert_eq!(
        requests[0].body,
        "grant_type=refresh_token&refresh_token=old-refresh&resource=https%3A%2F%2Fmcp.example%2F&client_id=client%20id"
    );
    assert_eq!(refreshed.access_token.as_str(), "new-access");
    assert_eq!(
        refreshed.refresh_token.as_deref().map(String::as_str),
        Some("old-refresh")
    );
    assert_eq!(refreshed.scope, "read");
    assert_eq!(refreshed.token_type, "bearer");
    assert!((before + 3_600_000..=now_ms() + 3_600_000).contains(&refreshed.expires_at_ms));
    let (rotated, _) = refresh_with(
        json(
            200,
            r#"{"access_token":"a","refresh_token":"rotated","scope":"read write"}"#,
        ),
        "none",
        None,
    )
    .await;
    let rotated = rotated.unwrap();
    assert_eq!(
        rotated.refresh_token.as_deref().map(String::as_str),
        Some("rotated")
    );
    assert_eq!(rotated.scope, "read write");
    assert_eq!(rotated.expires_at_ms, i64::MAX);
}

#[tokio::test]
async fn client_secrets_go_in_the_basic_header_or_the_form() {
    let accepted = r#"{"access_token":"a"}"#;
    let (basic, requests) = refresh_with(
        json(200, accepted),
        "client_secret_basic",
        Some("p@ss:wörd"),
    )
    .await;
    basic.unwrap();
    assert_eq!(
        requests[0].header("authorization"),
        Some(
            format!(
                "Basic {}",
                STANDARD.encode("client%20id:p%40ss%3Aw%C3%B6rd")
            )
            .as_str()
        )
    );
    assert!(!requests[0].body.contains("client"));
    let (post, requests) =
        refresh_with(json(200, accepted), "client_secret_post", Some("s3cret")).await;
    post.unwrap();
    assert!(
        requests[0]
            .body
            .ends_with("&client_id=client%20id&client_secret=s3cret")
    );
    for method in ["client_secret_basic", "client_secret_post"] {
        let (missing, requests) = refresh_with(json(200, accepted), method, None).await;
        assert_eq!(missing, Err(McpError::ClientSecretMissing));
        assert!(requests.is_empty());
    }
    let (unsupported, requests) =
        refresh_with(json(200, accepted), "private_key_jwt", Some("x")).await;
    assert_eq!(
        unsupported,
        Err(McpError::UnsupportedTokenEndpointAuthenticationMethod)
    );
    assert!(requests.is_empty());
}

#[tokio::test]
async fn only_invalid_grant_rejects_the_refresh_for_good() {
    for (answer, expected) in [
        (
            json(400, r#"{"error":"invalid_grant"}"#),
            McpError::McpRefreshRejected,
        ),
        (
            json(400, r#"{"error":"invalid_request"}"#),
            McpError::McpRefreshUnavailable,
        ),
        (
            plain(400, None, "invalid_grant"),
            McpError::McpRefreshUnavailable,
        ),
        (
            json(401, r#"{"error":"invalid_grant"}"#),
            McpError::McpRefreshUnavailable,
        ),
        (json(429, "{}"), McpError::McpRefreshUnavailable),
        (json(503, "{}"), McpError::McpRefreshUnavailable),
    ] {
        assert_eq!(refresh_with(answer, "none", None).await.0, Err(expected));
    }
}

#[tokio::test]
async fn token_responses_are_validated() {
    for (answer, expected) in [
        (
            plain(200, Some("text/plain"), r#"{"access_token":"a"}"#),
            McpError::InvalidOAuthResponseContentType,
        ),
        (
            plain(200, None, r#"{"access_token":"a"}"#),
            McpError::InvalidOAuthResponseContentType,
        ),
        (json(200, "[]"), McpError::InvalidTokenResponse),
        (json(200, "{"), McpError::InvalidTokenResponse),
        (
            json(200, r#"{"access_token":""}"#),
            McpError::InvalidOAuthResponse,
        ),
        (
            json(200, r#"{"access_token":"a","refresh_token":5}"#),
            McpError::InvalidOAuthResponse,
        ),
        (
            json(200, r#"{"access_token":"a","token_type":"mac"}"#),
            McpError::InvalidTokenResponse,
        ),
        (
            json(200, r#"{"access_token":"a","expires_in":-1}"#),
            McpError::InvalidTokenResponse,
        ),
        (
            json(200, r#"{"access_token":"a","expires_in":1.5}"#),
            McpError::InvalidTokenResponse,
        ),
        (
            json(
                200,
                &format!(r#"{{"access_token":"{}"}}"#, "x".repeat(MAX_DOCUMENT_BYTES)),
            ),
            McpError::McpAuthDocumentTooLarge,
        ),
    ] {
        assert_eq!(refresh_with(answer, "none", None).await.0, Err(expected));
    }
}

#[tokio::test]
async fn a_refresh_needs_a_refresh_token_and_a_secure_token_endpoint() {
    let mut missing = credentials("https://issuer.example/token", "none", None);
    missing.refresh_token = None;
    let http = http();
    assert_eq!(
        refresh_credentials(&http, &missing).await,
        Err(McpError::McpRefreshTokenMissing)
    );
    for (endpoint, expected) in [
        (
            "http://issuer.example/token",
            McpError::InsecureMcpAuthEndpoint,
        ),
        (
            "https://user@issuer.example/token",
            McpError::InsecureMcpAuthEndpoint,
        ),
        (
            "https://issuer.example/token#x",
            McpError::InsecureMcpAuthEndpoint,
        ),
        ("token", McpError::InvalidMcpAuthEndpoint),
    ] {
        assert_eq!(
            refresh_credentials(&http, &credentials(endpoint, "none", None)).await,
            Err(expected),
            "{endpoint}"
        );
    }
}

#[test]
fn credentials_need_a_refresh_a_minute_before_they_expire() {
    let mut entry = credentials("https://issuer.example/token", "none", None);
    entry.expires_at_ms = 100_000;
    assert!(!entry.needs_refresh(39_999));
    assert!(entry.needs_refresh(40_000));
    entry.expires_at_ms = i64::MIN;
    assert!(entry.needs_refresh(i64::MIN));
    assert_eq!(entry.bearer().as_str(), "Bearer old-access");
}
