use super::*;

struct FakeResolver {
    lookups: Mutex<VecDeque<CapabilityLookup>>,
    models: Mutex<Vec<String>>,
}

impl FakeResolver {
    fn new(lookups: Vec<CapabilityLookup>) -> Arc<Self> {
        Arc::new(Self {
            lookups: Mutex::new(lookups.into()),
            models: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.models.lock().unwrap().len()
    }

    fn models(&self) -> Vec<String> {
        self.models.lock().unwrap().clone()
    }
}

impl CapabilityResolver for FakeResolver {
    fn resolve<'a>(
        &'a self,
        model: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup> {
        self.models.lock().unwrap().push(model.to_owned());
        let lookup = self.lookups.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { lookup })
    }
}

fn supporting(efforts: &[&str], fast: bool) -> CapabilityLookup {
    CapabilityLookup::Resolved(ModelCapabilities {
        reasoning_efforts: efforts.iter().map(|effort| (*effort).to_owned()).collect(),
        supports_fast_mode: fast,
        context_window: None,
    })
}

fn requesting(effort: Option<&str>, fast: bool) -> AgentConfig {
    AgentConfig {
        reasoning_effort: effort.map(str::to_owned),
        fast_mode: fast,
        ..config()
    }
}

fn agent_with(
    provider: &Arc<FakeProvider>,
    resolver: Option<&Arc<FakeResolver>>,
    config: AgentConfig,
) -> Agent {
    let shared: Arc<FakeProvider> = Arc::clone(provider);
    let agent = Agent::new(
        shared,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        config,
    );
    match resolver {
        Some(resolver) => {
            let shared: Arc<FakeResolver> = Arc::clone(resolver);
            agent.with_capability_resolver(shared)
        }
        None => agent,
    }
}

fn sent_options(provider: &FakeProvider) -> Vec<(Option<String>, bool)> {
    provider
        .requests()
        .into_iter()
        .map(|request| (request.reasoning_effort, request.fast))
        .collect()
}

fn operational(events: &[UiEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::Operational { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn supported_effort_and_fast_mode_reach_every_request() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        text_reply("done"),
        text_reply("again"),
    ]);
    let resolver = FakeResolver::new(vec![supporting(&["low", "high"], true)]);
    let mut agent = agent_with(&provider, Some(&resolver), requesting(Some("high"), true));
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    run(&mut agent, "more").await;
    let high = (Some("high".to_owned()), true);
    assert_eq!(sent_options(&provider), [high.clone(), high.clone(), high]);
    assert_eq!(resolver.calls(), 1);
    assert!(operational(&events).is_empty());
}

#[tokio::test]
async fn a_switched_model_resolves_its_own_capabilities() {
    let provider = FakeProvider::new(vec![
        text_reply("one"),
        text_reply("two"),
        text_reply("three"),
    ]);
    let resolver = FakeResolver::new(vec![
        supporting(&["high"], false),
        supporting(&["low"], false),
    ]);
    let mut agent = agent_with(&provider, Some(&resolver), requesting(Some("high"), false));
    run(&mut agent, "one").await;
    agent.set_config(requesting(Some("high"), false));
    run(&mut agent, "two").await;
    agent.set_config(AgentConfig {
        model: "next-model".to_owned(),
        ..requesting(Some("low"), false)
    });
    run(&mut agent, "three").await;
    assert_eq!(resolver.models(), ["test-model", "next-model"]);
    let high = (Some("high".to_owned()), false);
    assert_eq!(
        sent_options(&provider),
        [high.clone(), high, (Some("low".to_owned()), false)]
    );
}

#[tokio::test]
async fn unsupported_settings_are_dropped_without_a_notice() {
    for lookup in [
        supporting(&["low", "high"], false),
        CapabilityLookup::Resolved(ModelCapabilities::default()),
    ] {
        let provider = FakeProvider::new(vec![text_reply("done")]);
        let resolver = FakeResolver::new(vec![lookup]);
        let mut agent = agent_with(&provider, Some(&resolver), requesting(Some("HIGH"), true));
        let (_, events) = run(&mut agent, "go").await;
        assert_eq!(sent_options(&provider), [(None, false)]);
        assert!(operational(&events).is_empty());
    }
}

#[tokio::test]
async fn capabilities_are_only_looked_up_for_an_effort_or_fast_mode() {
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let resolver = FakeResolver::new(Vec::new());
    let mut agent = agent_with(&provider, Some(&resolver), config());
    run(&mut agent, "go").await;
    assert_eq!(resolver.calls(), 0);
    assert_eq!(sent_options(&provider), [(None, false)]);
}

#[tokio::test]
async fn without_a_resolver_effort_and_fast_mode_are_dropped_quietly() {
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let mut agent = agent_with(&provider, None, requesting(Some("low"), true));
    let (_, events) = run(&mut agent, "go").await;
    assert_eq!(sent_options(&provider), [(None, false)]);
    assert!(operational(&events).is_empty());
}

#[tokio::test]
async fn an_unavailable_catalog_drops_fast_mode_with_one_notice_per_turn() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        text_reply("done"),
        text_reply("again"),
    ]);
    let resolver = FakeResolver::new(vec![CapabilityLookup::CatalogUnavailable]);
    let mut agent = agent_with(&provider, Some(&resolver), requesting(Some("low"), true));
    let (report, first) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let (_, second) = run(&mut agent, "more").await;
    let notice =
        "Fast mode is unavailable for this model right now; continuing at standard speed.\n";
    assert_eq!(operational(&first), [notice]);
    assert_eq!(operational(&second), [notice]);
    assert_eq!(
        sent_options(&provider),
        [(None, false), (None, false), (None, false)]
    );
    assert_eq!(resolver.calls(), 1);
}

#[tokio::test]
async fn an_unavailable_catalog_stays_quiet_when_fast_mode_is_off() {
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let resolver = FakeResolver::new(vec![CapabilityLookup::CatalogUnavailable]);
    let mut agent = agent_with(&provider, Some(&resolver), requesting(Some("low"), false));
    let (_, events) = run(&mut agent, "go").await;
    assert!(operational(&events).is_empty());
    assert_eq!(sent_options(&provider), [(None, false)]);
}

#[tokio::test]
async fn a_cancelled_lookup_interrupts_the_turn_and_is_retried_next_turn() {
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let resolver = FakeResolver::new(vec![
        CapabilityLookup::Cancelled,
        supporting(&["low"], false),
    ]);
    let mut agent = agent_with(&provider, Some(&resolver), requesting(Some("low"), false));
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(provider.requests().is_empty());
    let (report, _) = run(&mut agent, "again").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(resolver.calls(), 2);
    assert_eq!(sent_options(&provider), [(Some("low".to_owned()), false)]);
}

#[tokio::test(start_paused = true)]
async fn provider_outages_turn_fast_mode_off_for_the_rest_of_the_turn() {
    let provider = FakeProvider::new(vec![
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::RateLimited, "rate_limited"),
        ),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::BadGateway, "bad_gateway"),
        ),
        tool_reply(&[("call-1", "{}")]),
        text_reply("done"),
        text_reply("again"),
    ]);
    let resolver = FakeResolver::new(vec![supporting(&["low"], true)]);
    let mut agent = agent_with(&provider, Some(&resolver), requesting(Some("low"), true));
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    run(&mut agent, "more").await;
    let low = |fast| (Some("low".to_owned()), fast);
    assert_eq!(
        sent_options(&provider),
        [low(true), low(true), low(false), low(false), low(true)]
    );
    assert!(operational(&events).is_empty());
}

#[tokio::test(start_paused = true)]
async fn connection_retries_keep_fast_mode() {
    let provider = FakeProvider::new(vec![
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::ConnectivityLost, "ConnectionFailed"),
        ),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::TransportInterrupted, "ReadFailed"),
        ),
        text_reply("done"),
    ]);
    let resolver = FakeResolver::new(vec![supporting(&[], true)]);
    let mut agent = agent_with(&provider, Some(&resolver), requesting(None, true));
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        sent_options(&provider),
        [(None, true), (None, true), (None, true)]
    );
}

#[tokio::test]
async fn an_outage_after_reply_text_fails_the_turn_without_a_slower_retry() {
    let provider = FakeProvider::new(vec![
        Script::Fail(
            vec![StreamEvent::TextDelta {
                text: "partial".to_owned(),
            }],
            failure(ProviderErrorKind::ServerError, "server_error"),
        ),
        text_reply("never"),
    ]);
    let resolver = FakeResolver::new(vec![supporting(&[], true)]);
    let mut agent = agent_with(&provider, Some(&resolver), requesting(None, true));
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(sent_options(&provider), [(None, true)]);
}
