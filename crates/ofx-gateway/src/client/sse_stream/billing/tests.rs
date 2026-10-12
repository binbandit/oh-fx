use std::collections::VecDeque;

use ofx_contract::{DuplicateKeys, ProviderError, StreamEvent, parse_strict_json};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::super::{GatewayCompletion, Stream};
use super::*;
use crate::chat_completions::ChunkSource;

const GENERATION: &str = "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_GENERATION: &str = "gen_0123456789ABCDEFGHJKMNPQRS";
const STARTED: &str = "2026-04-01T12:11:07.123456Z";
const STARTED_MS: i64 = 1_775_045_467_123;

struct Chunks(VecDeque<Vec<u8>>);

impl ChunkSource for Chunks {
    async fn next_chunk(&mut self) -> Result<Option<impl AsRef<[u8]> + Send>, String> {
        Ok(self.0.pop_front())
    }
}

async fn consume(events: &[Value]) -> Result<GatewayCompletion, ProviderError> {
    let wire = events.iter().fold(String::new(), |mut wire, event| {
        wire.push_str("data: ");
        wire.push_str(&event.to_string());
        wire.push_str("\n\n");
        wire
    });
    let mut sink = |_: StreamEvent| {};
    let stream = Stream {
        requested_model: "test/model",
        secrets: &[],
    };
    let chunks = wire.as_bytes().chunks(11).map(<[u8]>::to_vec).collect();
    stream
        .consume(&mut Chunks(chunks), &mut sink, &CancellationToken::new())
        .await
}

fn metadata() -> Value {
    json!({"type": "response-metadata", "timestamp": STARTED, "modelId": "vendor/model-1"})
}

fn finish(gateway: &Value) -> Value {
    json!({
        "type": "finish",
        "finishReason": {"unified": "stop"},
        "usage": {
            "inputTokens": {"total": 120, "cacheRead": 100, "cacheWrite": 4},
            "outputTokens": {"total": 30, "reasoning": 12},
        },
        "providerMetadata": {"gateway": gateway},
    })
}

fn billed() -> Value {
    json!({
        "generationId": GENERATION,
        "cost": "0.0125",
        "routing": {"canonicalSlug": "vendor/model-1", "resolvedProvider": "bedrock"},
    })
}

fn expected() -> ProviderBilling {
    ProviderBilling {
        generation_id: GENERATION.to_owned(),
        created_at_ms: STARTED_MS,
        model: "vendor/model-1".to_owned(),
        total_cost: 0.0125,
        input_tokens: 120,
        output_tokens: 30,
        cache_read_tokens: 100,
        cache_write_tokens: 4,
        reasoning_tokens: Some(12),
        billable_web_search_calls: 0,
    }
}

fn object(value: &Value) -> Object<'static> {
    let text = Box::leak(value.to_string().into_boxed_str());
    parse_strict_json(text.as_bytes(), DuplicateKeys::AfterValue)
        .unwrap()
        .as_object()
        .unwrap()
        .clone()
}

#[test]
fn gateway_timestamps_parse_utc_fractions_strictly() {
    assert_eq!(
        parse_gateway_timestamp("2026-04-01T12:11:07Z"),
        Some(1_775_045_467_000)
    );
    assert_eq!(parse_gateway_timestamp(STARTED), Some(STARTED_MS));
    assert_eq!(
        parse_gateway_timestamp("2026-04-01T12:11:07.5Z"),
        Some(1_775_045_467_500)
    );
    assert_eq!(
        parse_gateway_timestamp("2024-02-29T00:00:00Z"),
        Some(1_709_164_800_000)
    );
    for invalid in [
        "2026-02-30T12:11:07Z",
        "2023-02-29T00:00:00Z",
        "1969-12-31T23:59:59Z",
        "2026-04-01T12:11:07",
        "2026-04-01T12:11:07+00:00",
        "2026-04-01T24:00:00Z",
        "2026-04-01T12:11:07.Z",
        "2026-04-01T12:11:07.1234567890Z",
        "2026-04-01 12:11:07Z",
        "2026-4-01T12:11:07ZZ",
        "+026-04-01T12:11:07Z",
    ] {
        assert_eq!(parse_gateway_timestamp(invalid), None, "{invalid}");
    }
}

#[tokio::test]
async fn a_finish_with_gateway_metadata_carries_exact_billing() {
    let completion = consume(&[metadata(), finish(&billed())]).await.unwrap();
    assert_eq!(completion.billing, Some(expected()));
}

#[tokio::test]
async fn provider_run_searches_count_as_billable_web_searches() {
    let search = |id: &str, name: &str, provider: bool| json!({"type": "tool-call", "toolCallId": id, "toolName": name, "input": "{}", "providerExecuted": provider});
    let completion = consume(&[
        metadata(),
        search("s1", "web_search", true),
        search("s2", "perplexity_search", true),
        search("s3", "web_search", false),
        search("s4", "fetch", true),
        finish(&billed()),
    ])
    .await
    .unwrap();
    assert_eq!(
        completion
            .billing
            .map(|billing| billing.billable_web_search_calls),
        Some(2)
    );
}

#[tokio::test]
async fn billing_needs_a_valid_timestamp_generation_and_terminal_metadata() {
    let without = |key: &str| {
        let mut gateway = billed();
        gateway.as_object_mut().unwrap().remove(key);
        gateway
    };
    let with = |key: &str, value: Value| {
        let mut gateway = billed();
        gateway[key] = value;
        gateway
    };
    let cases = [
        vec![finish(&billed())],
        vec![
            json!({"type": "response-metadata", "timestamp": "yesterday"}),
            finish(&billed()),
        ],
        vec![
            metadata(),
            json!({"type": "response-metadata", "timestamp": "2026-04-01T12:11:08Z"}),
            finish(&billed()),
        ],
        vec![metadata(), finish(&without("generationId"))],
        vec![metadata(), finish(&with("generationId", json!("gen_bad")))],
        vec![
            metadata(),
            json!({"type": "start-step", "providerMetadata": {"gateway": {"generationId": OTHER_GENERATION}}}),
            finish(&billed()),
        ],
        vec![
            metadata(),
            json!({"type": "response-metadata", "modelId": "vendor/model-2"}),
            finish(&billed()),
        ],
        vec![
            metadata(),
            json!({"type": "start-step", "providerMetadata": []}),
            finish(&billed()),
        ],
        vec![metadata(), finish(&without("cost"))],
        vec![metadata(), finish(&with("cost", json!(0.5)))],
        vec![metadata(), finish(&with("cost", json!("-1")))],
        vec![metadata(), finish(&with("cost", json!("inf")))],
        vec![metadata(), finish(&without("routing"))],
        vec![
            metadata(),
            finish(&with("routing", json!({"canonicalSlug": "vendor/model 1"}))),
        ],
        vec![
            metadata(),
            finish(&with("routing", json!({"canonicalSlug": ""}))),
        ],
    ];
    for events in cases {
        let completion = consume(&events).await.unwrap();
        assert_eq!(completion.billing, None, "{events:?}");
    }
}

#[test]
fn terminal_usage_must_hold_whole_consistent_counts() {
    let generation = Generation {
        id: Some(GENERATION.to_owned()),
        created_at_ms: Some(STARTED_MS),
        ..Generation::default()
    };
    let tools = ToolStream::default();
    let with_usage = |usage: Value| {
        let mut event = finish(&billed());
        event["usage"] = usage;
        object(&event)
    };
    assert_eq!(
        generation.billing(
            &with_usage(
                json!({"inputTokens": {"total": 5}, "outputTokens": {"total": 2, "reasoning": null}})
            ),
            &tools
        ),
        Some(ProviderBilling {
            input_tokens: 5,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            ..expected()
        })
    );
    for usage in [
        json!({"inputTokens": {"total": 5}}),
        json!({"inputTokens": {"total": -1}, "outputTokens": {"total": 2}}),
        json!({"inputTokens": {"total": 5.0}, "outputTokens": {"total": 2}}),
        json!({"inputTokens": {"total": 5, "cacheRead": 6}, "outputTokens": {"total": 2}}),
        json!({"inputTokens": {"total": 5, "cacheWrite": 6}, "outputTokens": {"total": 2}}),
        json!({"inputTokens": {"total": 5}, "outputTokens": {"total": 2, "reasoning": 3}}),
        json!({"inputTokens": {"total": 5}, "outputTokens": {"total": 2, "reasoning": "1"}}),
        json!({"inputTokens": 5, "outputTokens": {"total": 2}}),
    ] {
        assert_eq!(
            generation.billing(&with_usage(usage.clone()), &tools),
            None,
            "{usage}"
        );
    }
}
