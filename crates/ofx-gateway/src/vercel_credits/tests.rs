use ofx_testkit::{FakeServer, RefusedPort, Reply};

use super::*;

const KEY: &str = "vck_live_0123456789abcdef";

fn endpoints(server: &FakeServer) -> GatewayCreditsEndpoints {
    GatewayCreditsEndpoints {
        credits: format!("http://{}{GATEWAY_CREDITS_PATH}", server.address()),
    }
}

async fn fetched(server: &FakeServer, team: Option<&str>) -> CreditsSnapshot {
    fetch_credits(
        "oh-fx/test",
        &endpoints(server),
        Some(KEY),
        team,
        &CancellationToken::new(),
    )
    .await
}

#[tokio::test]
async fn credits_read_the_string_fields_and_send_the_key() {
    let server = FakeServer::start([Reply::status(
        200,
        r#"{"balance":"12.50","used":7.5,"plan":"pro","extra":true}"#,
    )]);
    assert_eq!(
        fetched(&server, None).await,
        CreditsSnapshot {
            balance: Some("12.50".to_owned()),
            used: None,
            plan: Some("pro".to_owned()),
            error: None,
        }
    );
    let request = &server.requests()[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, GATEWAY_CREDITS_PATH);
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(request.header("x-vercel-ai-gateway-team"), None);
}

#[tokio::test]
async fn a_signed_in_team_is_named_in_the_query_only_when_it_is_url_safe() {
    for (team, path) in [
        (
            Some("team_1"),
            format!("{GATEWAY_CREDITS_PATH}?teamId=team_1"),
        ),
        (Some("team one"), GATEWAY_CREDITS_PATH.to_owned()),
        (Some(""), GATEWAY_CREDITS_PATH.to_owned()),
    ] {
        let server = FakeServer::start([Reply::status(200, "{}")]);
        let snapshot = fetched(&server, team).await;
        assert_eq!(snapshot, CreditsSnapshot::default(), "{team:?}");
        assert_eq!(server.requests()[0].path, path, "{team:?}");
    }
}

#[tokio::test]
async fn failures_carry_upstream_messages_without_the_key() {
    for (reply, expected) in [
        (
            Reply::status(200, "not json"),
            "invalid JSON response from gateway",
        ),
        (
            Reply::status(200, "[]"),
            "unexpected response format from gateway",
        ),
        (
            Reply::status(200, r#"{"balance":"1","balance":"2"}"#),
            "invalid JSON response from gateway",
        ),
    ] {
        let server = FakeServer::start([reply]);
        assert_eq!(
            fetched(&server, None).await,
            CreditsSnapshot::failed(expected)
        );
    }
    let server = FakeServer::start([Reply::status(
        401,
        format!(r#"{{"error":{{"message":"key {KEY} is not valid"}}}}"#),
    )]);
    let error = fetched(&server, None).await.error.unwrap();
    assert!(error.starts_with("API access denied"), "{error}");
    assert!(!error.contains(KEY), "{error}");
    let refused = RefusedPort::reserve();
    let snapshot = fetch_credits(
        "oh-fx/test",
        &GatewayCreditsEndpoints {
            credits: format!("{}/credits", refused.base_url()),
        },
        None,
        None,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(
        snapshot,
        CreditsSnapshot::failed("failed to fetch credits from gateway")
    );
}
