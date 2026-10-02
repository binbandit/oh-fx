use super::*;

const CATALOG_MODEL: &str = "gpt-6.1-sol";
const FAST_UNAVAILABLE: &str =
    "Fast mode is unavailable for this model right now; continuing at standard speed.\n";

fn release() -> Reply {
    Reply::status(200, json!({"version": "0.153.1"}).to_string())
}

fn catalog_reply() -> Reply {
    Reply::status(
        200,
        json!({"models": [{
            "slug": CATALOG_MODEL,
            "visibility": "list",
            "supported_in_api": true,
            "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}],
            "additional_speed_tiers": ["fast"],
        }]})
        .to_string(),
    )
}

fn endpoints(codex: &FakeServer, catalog: &FakeServer) -> SubscriptionEndpoints {
    let unused = FakeServer::start([]);
    SubscriptionEndpoints {
        models: CodexModelsEndpoints {
            models: format!("{}/backend-api/codex/models", catalog.base_url()),
            client_version: format!("{}/@openai/codex/latest", catalog.base_url()),
        },
        ..subscription_endpoints(&unused, codex)
    }
}

async fn ask(
    fixture: &Fixture,
    codex: &FakeServer,
    catalog: &FakeServer,
    effort: Option<&str>,
    fast: bool,
) -> (TurnReport, Vec<UiEvent>) {
    let subscription = fixture
        .subscription(endpoints(codex, catalog))
        .await
        .expect("the saved login builds a subscription");
    let agent = fixture.agent_with(
        subscription.provider,
        agent_config(CATALOG_MODEL, effort, fast),
    );
    let mut agent = agent.with_capability_resolver(Arc::new(subscription.capabilities));
    run(&mut agent, "say hi").await
}

fn operational(seen: &[UiEvent]) -> Vec<&str> {
    seen.iter()
        .filter_map(|event| match event {
            UiEvent::Operational { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_supported_effort_and_fast_mode_reach_the_responses_request() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let codex = FakeServer::start([Reply::sse(&text_events("hi"))]);
    let catalog = FakeServer::start([release(), catalog_reply()]);
    let (report, seen) = ask(&fixture, &codex, &catalog, Some("low"), true).await;
    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert!(operational(&seen).is_empty());

    let lookups = catalog.requests();
    assert_eq!(lookups.len(), 2);
    assert_eq!(
        lookups[1].path,
        "/v1/backend-api/codex/models?client_version=0.153.1"
    );
    let bearer = format!("Bearer {SAVED_TOKEN}");
    assert_eq!(lookups[1].header("authorization"), Some(bearer.as_str()));
    assert_eq!(lookups[1].header("chatgpt-account-id"), Some(ACCOUNT));

    let requests = codex.requests();
    assert_eq!(requests.len(), 1);
    assert_codex_request(&requests[0], SAVED_TOKEN);
    let body = requests[0].json();
    assert_eq!(body["model"], CATALOG_MODEL);
    assert_eq!(body["service_tier"], "priority");
    assert_eq!(
        body["reasoning"],
        json!({"effort": "low", "summary": "auto"})
    );
    assert!(
        fixture
            .paths
            .cache
            .join("provider-versions/codex.json")
            .is_file()
    );
}

#[tokio::test]
async fn settings_the_model_does_not_list_are_left_out() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let codex = FakeServer::start([Reply::sse(&text_events("hi"))]);
    let catalog = FakeServer::start([release(), catalog_reply()]);
    let (report, seen) = ask(&fixture, &codex, &catalog, Some("xhigh"), false).await;
    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert!(operational(&seen).is_empty());
    let body = codex.requests()[0].json();
    assert_eq!(body.get("reasoning"), None);
    assert_eq!(body.get("service_tier"), None);
}

#[tokio::test]
async fn an_unavailable_catalog_drops_fast_mode_with_a_notice() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let codex = FakeServer::start([Reply::sse(&text_events("hi"))]);
    let catalog = FakeServer::start([release(), Reply::status(503, "{}")]);
    let (report, seen) = ask(&fixture, &codex, &catalog, Some("low"), true).await;
    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert_eq!(operational(&seen), [FAST_UNAVAILABLE]);
    let body = codex.requests()[0].json();
    assert_eq!(body.get("reasoning"), None);
    assert_eq!(body.get("service_tier"), None);
}

#[tokio::test]
async fn without_an_effort_or_fast_mode_the_catalog_is_never_fetched() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let codex = FakeServer::start([Reply::sse(&text_events("hi"))]);
    let catalog = FakeServer::start([]);
    let (report, _) = ask(&fixture, &codex, &catalog, None, false).await;
    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert!(catalog.requests().is_empty());
}
