use ofx_config::ProviderId;
use ofx_contract::ProviderBilling;

use super::*;
use crate::session_codec::SavedProvider;
use crate::session_usage::Usage;

const NOW: i64 = 1_775_045_467_000;

fn billing(generation_id: &str, model: &str) -> ProviderBilling {
    ProviderBilling {
        generation_id: generation_id.to_owned(),
        created_at_ms: 100,
        model: model.to_owned(),
        total_cost: 0.25,
        input_tokens: 17,
        output_tokens: 7,
        cache_read_tokens: 2,
        cache_write_tokens: 0,
        reasoning_tokens: Some(1),
        billable_web_search_calls: 0,
    }
}

fn exact(usage: &mut Usage, generation_id: &str, model: &str) {
    let sequence = usage.reserve(NOW).unwrap();
    let provider = SavedProvider::new(ProviderId::Codex, None).unwrap();
    usage.finish_exact(sequence, 3, &provider, &billing(generation_id, model), NOW);
}

fn publish_all(usage: &mut Usage) {
    let batch = usage.publication_batch(NOW);
    for fact in &batch.facts {
        usage.fact_published(fact);
    }
}

#[test]
fn a_published_generation_settles_its_totals_and_model() {
    let mut usage = Usage::fresh();
    exact(&mut usage, "response-1", "codex/gpt-test");
    let batch = usage.publication_batch(NOW);
    assert_eq!(batch.pending.len(), 1);
    assert_eq!(batch.pending[0].observed_at_ms, NOW);
    assert_eq!(batch.facts.len(), 1);
    assert!(!batch.checkpoint_changed);
    usage.fact_published(&batch.facts[0]);

    let snapshot = usage.snapshot(NOW);
    assert_eq!(snapshot.billing, Availability::Complete);
    assert!(snapshot.pending.is_empty());
    assert!(snapshot.publication_backlog.is_empty());
    assert_eq!(
        (
            snapshot.input_tokens,
            snapshot.output_tokens,
            snapshot.cache_read_tokens,
            snapshot.reasoning_tokens,
            snapshot.request_count,
        ),
        (17, 7, 2, Some(1), Some(1))
    );
    assert!((snapshot.total_cost - 0.25).abs() < f64::EPSILON);
    assert_eq!(
        snapshot.models,
        [ModelAggregate {
            model: "codex/gpt-test".to_owned(),
            first_sequence: 1,
            total_cost: 0.25,
            input_tokens: 17,
            output_tokens: 7,
            cache_read_tokens: 2,
            cache_write_tokens: 0,
            reasoning_tokens: Some(1),
            request_count: Some(1),
            billable_web_search_calls: 0,
        }]
    );
    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn generations_aggregate_per_model_in_invocation_order() {
    let mut usage = Usage::fresh();
    exact(&mut usage, "response-1", "codex/b");
    exact(&mut usage, "response-2", "codex/a");
    exact(&mut usage, "response-3", "codex/b");
    let batch = usage.publication_batch(NOW);
    for fact in batch.facts.iter().rev() {
        usage.fact_published(fact);
    }
    let snapshot = usage.snapshot(NOW);
    let models: Vec<(&str, u64, u64, Option<u64>)> = snapshot
        .models
        .iter()
        .map(|model| {
            (
                model.model.as_str(),
                model.first_sequence,
                model.input_tokens,
                model.request_count,
            )
        })
        .collect();
    assert_eq!(
        models,
        [("codex/b", 1, 34, Some(2)), ("codex/a", 2, 17, Some(1))]
    );
    assert_eq!(snapshot.request_count, Some(3));
    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn usage_keeps_more_than_sixteen_exact_resolved_models_up_to_its_limit() {
    let mut usage = Usage::fresh();
    for index in 0..MAX_MODELS {
        exact(
            &mut usage,
            &format!("response-{index}"),
            &format!("codex/m{index}"),
        );
        publish_all(&mut usage);
    }
    let full = usage.snapshot(NOW);
    assert_eq!(full.models.len(), MAX_MODELS);
    assert_eq!(full.billing, Availability::Complete);
    assert_eq!(full.validate(), Ok(()));

    exact(&mut usage, "response-over", "codex/over");
    publish_all(&mut usage);
    let over = usage.snapshot(NOW);
    assert_eq!(over.models.len(), MAX_MODELS);
    assert_eq!(over.billing, Availability::Incomplete);
    assert!(over.publication_backlog.is_empty());
    assert_eq!(
        over.incidents,
        [UsageIncident {
            occurred_at_ms: 100,
            completeness: UsageCompleteness::Incomplete,
        }]
    );
    assert_eq!(over.validate(), Ok(()));
}

#[test]
fn a_published_record_without_its_pending_generation_only_leaves_the_backlog() {
    let mut snapshot = UsageSnapshot::fresh();
    let mut usage = Usage::fresh();
    exact(&mut usage, "response-1", "codex/gpt-test");
    let fact = usage.publication_batch(NOW).facts.remove(0);
    snapshot.publication_backlog.push(fact.clone());
    snapshot.settle_published(&fact);
    assert!(snapshot.publication_backlog.is_empty());
    assert_eq!(snapshot.input_tokens, 0);
    assert_eq!(snapshot.billing, Availability::Complete);
}

#[test]
fn totals_that_would_overflow_mark_billing_incomplete_and_drop_the_record() {
    let mut usage = Usage::fresh();
    exact(&mut usage, "response-1", "codex/gpt-test");
    let mut snapshot = usage.snapshot(NOW);
    snapshot.input_tokens = u64::MAX;
    let fact = snapshot.publication_backlog[0].clone();
    snapshot.settle_published(&fact);
    assert_eq!(snapshot.billing, Availability::Incomplete);
    assert!(snapshot.publication_backlog.is_empty());
    assert_eq!(snapshot.pending.len(), 1);
    assert_eq!(snapshot.input_tokens, u64::MAX);
    assert!(snapshot.models.is_empty());
}

#[test]
fn a_batch_dates_restored_pending_generations_and_published_incidents_leave() {
    let mut usage = Usage::fresh();
    exact(&mut usage, "response-1", "codex/gpt-test");
    usage.mark_billing_incomplete(NOW - 5);
    let mut snapshot = usage.snapshot(NOW);
    snapshot.pending[0].observed_at_ms = None;
    let batch = snapshot.publication_batch(NOW);
    assert!(batch.checkpoint_changed);
    assert_eq!(batch.pending[0].observed_at_ms, NOW);
    assert_eq!(snapshot.pending[0].observed_at_ms, Some(NOW));
    assert_eq!(batch.incidents.len(), 1);
    snapshot.remove_incident(&batch.incidents[0]);
    assert!(snapshot.incidents.is_empty());
    assert!(!snapshot.publication_batch(NOW).checkpoint_changed);
}
