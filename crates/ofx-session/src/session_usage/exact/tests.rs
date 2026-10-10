use super::*;
use crate::session_usage::Usage;

const NOW: i64 = 1_775_045_467_000;
const GATEWAY_ID: &str = "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn codex() -> SavedProvider {
    SavedProvider::new(ProviderId::Codex, None).unwrap()
}

fn portkey(first_binding_byte: u8) -> SavedProvider {
    let mut binding: [u8; 32] = std::array::from_fn(|index| u8::try_from(index).unwrap());
    binding[0] = first_binding_byte;
    SavedProvider::new(ProviderId::Configured("portkey".to_owned()), Some(binding)).unwrap()
}

fn billing(generation_id: &str) -> ProviderBilling {
    ProviderBilling {
        generation_id: generation_id.to_owned(),
        created_at_ms: 100,
        model: "codex/gpt-test".to_owned(),
        total_cost: 0.0,
        input_tokens: 17,
        output_tokens: 7,
        cache_read_tokens: 2,
        cache_write_tokens: 0,
        reasoning_tokens: Some(1),
        billable_web_search_calls: 0,
    }
}

fn canonical(provider: &SavedProvider, external_id: &str) -> Option<String> {
    canonical_exact_generation_id(provider, external_id)
}

fn settle(usage: &mut Usage, provider: &SavedProvider, billing: &ProviderBilling) {
    let sequence = usage.reserve(NOW).unwrap();
    usage.finish_exact(sequence, 3, provider, billing, NOW);
}

fn incomplete_at(occurred_at_ms: i64) -> UsageIncident {
    UsageIncident {
        occurred_at_ms,
        completeness: UsageCompleteness::Incomplete,
    }
}

#[test]
fn exact_generation_ids_are_deterministic_and_provider_scoped() {
    let grok = SavedProvider::new(ProviderId::Grok, None).unwrap();
    let gateway = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    for (provider, expected) in [
        (codex(), "gen_464AFFD5B342524F0E5B522622"),
        (grok, "gen_E25F24A18624442BDD99A1ED7C"),
    ] {
        let id = canonical(&provider, "response-shared-id").unwrap();
        assert_eq!(id, expected);
        assert!(valid_gateway_generation_id(&id));
    }
    assert_eq!(
        canonical(&portkey(0), "chatcmpl-1").as_deref(),
        Some("gen_2B3FF54FA70D933E0F269DCA1F")
    );
    assert_ne!(
        canonical(&portkey(0), "chatcmpl-1"),
        canonical(&portkey(1), "chatcmpl-1")
    );
    assert_eq!(canonical(&gateway, GATEWAY_ID).as_deref(), Some(GATEWAY_ID));
    assert_eq!(canonical(&gateway, "response-shared-id"), None);
    let too_long = "r".repeat(MAX_IDENTIFIER_BYTES + 1);
    for invalid in ["", "response\ninvalid", "response\u{7f}", &too_long] {
        assert_eq!(canonical(&codex(), invalid), None, "{invalid:?}");
    }
    assert!(canonical(&codex(), &"r".repeat(MAX_IDENTIFIER_BYTES)).is_some());
}

#[test]
fn an_exact_generation_waits_in_the_publication_backlog() {
    let mut usage = Usage::fresh();
    settle(&mut usage, &codex(), &billing("response-1"));
    let snapshot = usage.snapshot(NOW);
    let id = canonical(&codex(), "response-1").unwrap();
    assert_eq!(snapshot.billing, Availability::Pending);
    assert_eq!(
        snapshot.pending,
        [PendingGeneration {
            id: id.clone(),
            sequence: 1,
            provider: ProviderId::Codex,
            origin: "exact/codex".to_owned(),
            team: None,
            credential_source: None,
            credential_identity: None,
            account_id: None,
            observed_at_ms: Some(NOW),
        }]
    );
    assert_eq!(
        snapshot.publication_backlog,
        [exact_fact(&codex(), &billing("response-1")).unwrap()]
    );
    assert_eq!(snapshot.publication_backlog[0].id, id);
    assert_eq!(
        (snapshot.input_tokens, snapshot.request_count),
        (0, Some(0))
    );
    assert_eq!(snapshot.settled_through_sequence, 1);
    assert!(snapshot.api_duration_complete);
    assert!(snapshot.incidents.is_empty());
    assert_eq!(snapshot.validate(), Ok(()));

    let mut configured = Usage::fresh();
    settle(&mut configured, &portkey(0), &billing("chatcmpl-1"));
    let snapshot = configured.snapshot(NOW);
    assert_eq!(snapshot.pending[0].origin, "exact/configured");
    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn a_repeated_generation_identity_marks_billing_incomplete() {
    let mut usage = Usage::fresh();
    settle(&mut usage, &codex(), &billing("response-1"));
    settle(&mut usage, &codex(), &billing("response-1"));
    let snapshot = usage.snapshot(NOW);
    assert_eq!(snapshot.billing, Availability::Incomplete);
    assert_eq!(snapshot.pending.len(), 1);
    assert_eq!(snapshot.publication_backlog.len(), 1);
    assert_eq!(snapshot.incidents, [incomplete_at(NOW)]);
    assert_eq!(snapshot.settled_through_sequence, 2);
    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn billing_that_breaks_the_record_rules_settles_as_possibly_billed() {
    let changes: [fn(&mut ProviderBilling); 10] = [
        |billing: &mut ProviderBilling| billing.generation_id.clear(),
        |billing: &mut ProviderBilling| billing.generation_id.push('\n'),
        |billing: &mut ProviderBilling| billing.model = "codex/gpt test".to_owned(),
        |billing: &mut ProviderBilling| billing.model.clear(),
        |billing: &mut ProviderBilling| billing.cache_read_tokens = 18,
        |billing: &mut ProviderBilling| billing.cache_write_tokens = 18,
        |billing: &mut ProviderBilling| billing.reasoning_tokens = Some(8),
        |billing: &mut ProviderBilling| billing.created_at_ms = -1,
        |billing: &mut ProviderBilling| billing.total_cost = f64::NAN,
        |billing: &mut ProviderBilling| billing.total_cost = -0.5,
    ];
    for change in changes {
        let mut broken = billing("response-1");
        change(&mut broken);
        let mut usage = Usage::fresh();
        settle(&mut usage, &codex(), &broken);
        let snapshot = usage.snapshot(NOW);
        assert_eq!(snapshot.billing, Availability::Incomplete, "{broken:?}");
        assert!(snapshot.pending.is_empty(), "{broken:?}");
        assert!(snapshot.publication_backlog.is_empty(), "{broken:?}");
        assert_eq!(snapshot.incidents, [incomplete_at(NOW)], "{broken:?}");
        assert_eq!(snapshot.settled_through_sequence, 1, "{broken:?}");
    }
}

#[test]
fn the_sixteenth_pending_generation_is_the_capacity_boundary() {
    let mut usage = Usage::fresh();
    for index in 0..MAX_PENDING_GENERATIONS {
        settle(&mut usage, &codex(), &billing(&format!("response-{index}")));
    }
    let full = usage.snapshot(NOW);
    assert_eq!(full.billing, Availability::Pending);
    assert_eq!(full.pending.len(), MAX_PENDING_GENERATIONS);
    assert_eq!(full.publication_backlog.len(), MAX_PUBLICATION_BACKLOG);
    assert_eq!(full.validate(), Ok(()));

    settle(&mut usage, &codex(), &billing("response-over"));
    let over = usage.snapshot(NOW);
    assert_eq!(over.billing, Availability::Incomplete);
    assert_eq!(over.pending.len(), MAX_PENDING_GENERATIONS);
    assert_eq!(over.publication_backlog.len(), MAX_PUBLICATION_BACKLOG);
    assert_eq!(over.incidents, [incomplete_at(NOW)]);
    assert_eq!(over.settled_through_sequence, 17);
    assert_eq!(over.validate(), Ok(()));
}

#[test]
fn an_exact_settlement_for_an_unknown_request_stages_nothing() {
    let mut usage = Usage::fresh();
    usage.finish_exact(4, 3, &codex(), &billing("response-1"), NOW);
    let snapshot = usage.snapshot(NOW);
    assert_eq!(snapshot.billing, Availability::Incomplete);
    assert!(!snapshot.api_duration_complete);
    assert!(snapshot.pending.is_empty());
    assert!(snapshot.publication_backlog.is_empty());
}

#[test]
fn the_backlog_keeps_one_record_per_generation_and_its_limit() {
    let fact = exact_fact(&codex(), &billing("response-1")).unwrap();
    let mut snapshot = UsageSnapshot::fresh();
    snapshot.next_sequence = 2;
    assert_eq!(snapshot.stage_publication(fact.clone()), Ok(()));
    assert!(snapshot.publication_backlog.is_empty());

    snapshot
        .observe_exact(1, &fact.id, &ProviderId::Codex, NOW)
        .unwrap();
    assert_eq!(snapshot.stage_publication(fact.clone()), Ok(()));
    assert_eq!(snapshot.stage_publication(fact.clone()), Ok(()));
    assert_eq!(snapshot.publication_backlog, std::slice::from_ref(&fact));
    assert_eq!(snapshot.billing, Availability::Pending);

    let mut changed = fact.clone();
    changed.output_tokens += 1;
    assert_eq!(
        snapshot.stage_publication(changed),
        Err(UsageSnapshotError::Invalid)
    );
    assert_eq!(snapshot.billing, Availability::Incomplete);
    assert_eq!(snapshot.incidents, [incomplete_at(fact.created_at_ms)]);

    let mut full = UsageSnapshot::fresh();
    full.next_sequence = 2;
    full.publication_backlog = (0..MAX_PUBLICATION_BACKLOG)
        .map(|index| exact_fact(&codex(), &billing(&format!("other-{index}"))).unwrap())
        .collect();
    full.observe_exact(1, &fact.id, &ProviderId::Codex, NOW)
        .unwrap();
    assert_eq!(
        full.stage_publication(fact.clone()),
        Err(UsageSnapshotError::CapacityExceeded)
    );
    assert_eq!(full.billing, Availability::Incomplete);
    assert_eq!(full.publication_backlog.len(), MAX_PUBLICATION_BACKLOG);
}
