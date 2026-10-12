use ofx_contract::{UsageCompleteness, UsageIncident};

use super::*;
use crate::session_codec::SavedProvider;
use crate::session_usage::Usage;

const NOW: i64 = 1_775_045_467_000;
const GATEWAY_ID: &str = "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_ID: &str = "gen_01ARZ3NDEKTSV4RRFFQ69G5FAW";
const ORIGIN: &str = "https://ai-gateway.vercel.sh";

fn gateway() -> SavedProvider {
    SavedProvider::new(ProviderId::Gateway, None).unwrap()
}

fn observed(id: &str) -> ObservedGeneration {
    ObservedGeneration {
        id: id.to_owned(),
        origin: ORIGIN.to_owned(),
        team: Some("team_1".to_owned()),
    }
}

fn defer(
    usage: &mut Usage,
    generation: &ObservedGeneration,
    credential: RouteCredential,
) -> Result<bool, DeferredRefusal> {
    let sequence = usage.reserve(NOW).unwrap();
    usage.finish_deferred((sequence, 3), &gateway(), generation, credential, NOW)
}

fn incomplete() -> UsageIncident {
    UsageIncident {
        occurred_at_ms: NOW,
        completeness: UsageCompleteness::Incomplete,
    }
}

#[test]
fn a_deferred_generation_waits_as_pending_with_its_credential_authority() {
    let mut usage = Usage::fresh();
    let credential = RouteCredential::ai_gateway_api_key();
    assert_eq!(
        defer(&mut usage, &observed(GATEWAY_ID), credential),
        Ok(true)
    );
    let snapshot = usage.snapshot(NOW);
    assert_eq!(snapshot.billing, Availability::Pending);
    assert_eq!(
        snapshot.pending,
        [PendingGeneration {
            id: GATEWAY_ID.to_owned(),
            sequence: 1,
            provider: ProviderId::Gateway,
            origin: ORIGIN.to_owned(),
            team: Some("team_1".to_owned()),
            credential_source: Some("ai_gateway_api_key"),
            credential_identity: credential.identity(),
            account_id: None,
            observed_at_ms: Some(NOW),
        }]
    );
    assert!(credential.identity().is_some());
    assert!(snapshot.publication_backlog.is_empty());
    assert!(snapshot.incidents.is_empty());
    assert_eq!(snapshot.settled_through_sequence, 1);
    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn a_generation_seen_again_with_another_identity_is_refused() {
    let mut usage = Usage::fresh();
    let credential = RouteCredential::fx_login();
    assert_eq!(
        defer(&mut usage, &observed(GATEWAY_ID), credential),
        Ok(true)
    );
    let elsewhere = ObservedGeneration {
        origin: "http://127.0.0.1:9".to_owned(),
        ..observed(GATEWAY_ID)
    };
    assert_eq!(
        defer(&mut usage, &elsewhere, credential),
        Err(DeferredRefusal::Conflicting)
    );
    assert_eq!(
        defer(
            &mut usage,
            &observed(GATEWAY_ID),
            RouteCredential::stored_key()
        ),
        Err(DeferredRefusal::Conflicting)
    );
    let snapshot = usage.snapshot(NOW);
    assert_eq!(snapshot.billing, Availability::Incomplete);
    assert_eq!(snapshot.pending.len(), 1);
    assert_eq!(snapshot.incidents, [incomplete()]);
    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn malformed_identities_and_a_full_ledger_are_refused() {
    let cases = [
        (
            ObservedGeneration {
                id: "generation-1".to_owned(),
                ..observed(GATEWAY_ID)
            },
            DeferredRefusal::GenerationId,
        ),
        (
            ObservedGeneration {
                origin: "https://ai gateway".to_owned(),
                ..observed(GATEWAY_ID)
            },
            DeferredRefusal::Origin,
        ),
        (
            ObservedGeneration {
                team: Some(String::new()),
                ..observed(GATEWAY_ID)
            },
            DeferredRefusal::Team,
        ),
    ];
    for (generation, refusal) in cases {
        let mut usage = Usage::fresh();
        assert_eq!(
            defer(&mut usage, &generation, RouteCredential::fx_login()),
            Err(refusal)
        );
        let snapshot = usage.snapshot(NOW);
        assert_eq!(snapshot.billing, Availability::Incomplete, "{refusal}");
        assert!(snapshot.pending.is_empty(), "{refusal}");
        assert_eq!(snapshot.settled_through_sequence, 1, "{refusal}");
    }

    let mut usage = Usage::fresh();
    for index in 0..MAX_PENDING_GENERATIONS {
        let id = format!("gen_01ARZ3NDEKTSV4RRFFQ69G5F{index:02}");
        assert_eq!(
            defer(&mut usage, &observed(&id), RouteCredential::fx_login()),
            Ok(true)
        );
    }
    assert_eq!(
        defer(&mut usage, &observed(OTHER_ID), RouteCredential::fx_login()),
        Err(DeferredRefusal::CapacityExceeded)
    );
    let snapshot = usage.snapshot(NOW);
    assert_eq!(snapshot.pending.len(), MAX_PENDING_GENERATIONS);
    assert_eq!(snapshot.billing, Availability::Incomplete);
    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn a_deferred_settlement_for_an_unknown_request_is_ignored() {
    let mut usage = Usage::fresh();
    assert_eq!(
        usage.finish_deferred(
            (4, 3),
            &gateway(),
            &observed(GATEWAY_ID),
            RouteCredential::fx_login(),
            NOW
        ),
        Ok(false)
    );
    assert!(usage.snapshot(NOW).pending.is_empty());
}
