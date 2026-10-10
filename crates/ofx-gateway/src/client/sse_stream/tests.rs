use std::collections::VecDeque;

use super::*;

const KEY: &str = "vck_live_0123456789abcdef";

struct Chunks(VecDeque<Vec<u8>>);

impl ChunkSource for Chunks {
    async fn next_chunk(&mut self) -> Result<Option<impl AsRef<[u8]> + Send>, String> {
        Ok(self.0.pop_front())
    }
}

struct Failing;

impl ChunkSource for Failing {
    async fn next_chunk(&mut self) -> Result<Option<impl AsRef<[u8]> + Send>, String> {
        Err::<Option<Vec<u8>>, _>("connection reset by peer".to_owned())
    }
}

fn events(lines: &[&str]) -> String {
    lines.iter().fold(String::new(), |mut wire, line| {
        wire.push_str("data: ");
        wire.push_str(line);
        wire.push_str("\n\n");
        wire
    })
}

async fn consume_wire(
    wire: &str,
    cancel: &CancellationToken,
) -> (Result<GatewayCompletion, ProviderError>, Vec<StreamEvent>) {
    let mut seen = Vec::new();
    let mut sink = |event: StreamEvent| seen.push(event);
    let secrets = [KEY.to_owned()];
    let stream = Stream {
        requested_model: "test/model",
        secrets: &secrets,
    };
    let chunks = wire.as_bytes().chunks(7).map(<[u8]>::to_vec).collect();
    let result = stream.consume(&mut Chunks(chunks), &mut sink, cancel).await;
    (result, seen)
}

async fn consume(wire: &str) -> Result<GatewayCompletion, ProviderError> {
    consume_wire(wire, &CancellationToken::new()).await.0
}

#[tokio::test]
async fn framing_assembles_gateway_data_fields() {
    for answer in [
        "data:{\"type\":\"text-delta\",\"delta\":\"EXPECTED_FINAL\"}\n\n",
        "data: {\"type\":\"text-delta\",\ndata: \"delta\":\"EXPECTED_FINAL\"}\n\n",
    ] {
        let wire = format!(
            "data: {{\"type\":\"text-delta\",\"delta\":\"CONTROL_PREFIX\\n\"}}\n\n{answer}data: {{\"type\":\"finish\",\"finishReason\":{{\"unified\":\"stop\"}}}}\n\n"
        );
        let completion = consume(&wire).await.unwrap();
        assert_eq!(completion.content, "CONTROL_PREFIX\nEXPECTED_FINAL");
    }
}

#[tokio::test]
async fn the_finish_reason_and_usage_are_kept_and_later_events_are_not_read() {
    let wire = events(&[
        r#"{"type":"text-delta","id":"t1","delta":"Hola"}"#,
        r#"{"type":"finish","finishReason":{"unified":"length","raw":"length"},"usage":{"inputTokens":{"total":10},"outputTokens":{"total":5,"reasoning":2}}}"#,
        r#"{"type":"text-delta","id":"t2","delta":"late"}"#,
        "[DONE]",
    ]);
    let completion = consume(&wire).await.unwrap();
    assert_eq!(completion.content, "Hola");
    assert_eq!(completion.finish, Some(Finish::Length));
    assert_eq!(completion.usage.input_tokens, Some(10));
    assert_eq!(completion.usage.output_tokens, Some(5));
    assert_eq!(completion.events, 2);
}

#[tokio::test]
async fn malformed_usage_totals_are_ignored() {
    let wire = events(&[
        r#"{"type":"finish","finishReason":{"unified":"stop"},"usage":{"inputTokens":{"total":-1},"outputTokens":{"total":"5"}}}"#,
    ]);
    let completion = consume(&wire).await.unwrap();
    assert_eq!(completion.finish, Some(Finish::Stop));
    assert_eq!(completion.usage, Usage::default());
}

#[tokio::test]
async fn every_unified_finish_reason_is_read() {
    for (unified, finish) in [
        ("stop", Finish::Stop),
        ("length", Finish::Length),
        ("content-filter", Finish::ContentFilter),
        ("tool-calls", Finish::ToolCalls),
        ("error", Finish::ProviderError),
        ("other", Finish::Other),
    ] {
        let finish_event =
            format!(r#"{{"type":"finish","finishReason":{{"unified":"{unified}"}}}}"#);
        let completion = consume(&events(&[&finish_event])).await.unwrap();
        assert_eq!(completion.finish, Some(finish), "{unified}");
        assert_eq!(finish_label(completion.finish), unified);
    }
    assert_eq!(finish_label(None), "(none)");
}

#[tokio::test]
async fn malformed_finish_reasons_are_rejected() {
    for finish in [
        r#"{"type":"finish","finishReason":{"unified":""}}"#,
        r#"{"type":"finish","finishReason":{"unified":"future-reason"}}"#,
        r#"{"type":"finish","finishReason":"stop"}"#,
        r#"{"type":"finish"}"#,
    ] {
        let error = consume(&events(&[finish])).await.unwrap_err();
        assert_eq!(error.code, "InvalidProviderFinishReason", "{finish}");
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
    }
}

#[tokio::test]
async fn provider_error_details_keep_code_and_message() {
    let wire = events(&[
        r#"{"type":"error","error":{"code":"provider_down","message":"wafer route unavailable"}}"#,
        r#"{"type":"finish","finishReason":{"unified":"error","raw":"provider_error"},"usage":{"inputTokens":{"total":1},"outputTokens":{"total":1}}}"#,
    ]);
    let completion = consume(&wire).await.unwrap();
    assert_eq!(completion.finish, Some(Finish::ProviderError));
    assert_eq!(
        completion.failure_detail.as_deref(),
        Some("provider_down: wafer route unavailable")
    );
    assert_eq!(completion.failure_cause, None);
    assert_eq!(completion.usage.input_tokens, Some(1));
}

#[tokio::test]
async fn a_gateway_stream_timeout_is_classified_by_its_structured_code() {
    let wire = events(&[
        r#"{"type":"error","error":{"code":"gateway_stream_timeout","message":"stream exceeded maximum duration"}}"#,
    ]);
    let completion = consume(&wire).await.unwrap();
    assert_eq!(completion.finish, None);
    assert_eq!(
        completion.failure_cause,
        Some(FailureCause::GatewayStreamTimeout)
    );
    assert_eq!(
        completion.failure_detail.as_deref(),
        Some("gateway_stream_timeout: stream exceeded maximum duration")
    );
    let finish_only = events(&[
        r#"{"type":"finish","finishReason":{"unified":"error","raw":"gateway_stream_timeout"}}"#,
    ]);
    let completion = consume(&finish_only).await.unwrap();
    assert_eq!(completion.finish, Some(Finish::ProviderError));
    assert_eq!(
        completion.failure_cause,
        Some(FailureCause::GatewayStreamTimeout)
    );
}

#[tokio::test]
async fn matching_prose_without_the_structured_code_is_not_a_timeout() {
    let wire = events(&[
        r#"{"type":"error","error":{"code":"provider_error","message":"gateway_stream_timeout"}}"#,
        r#"{"type":"finish","finishReason":{"unified":"error"}}"#,
    ]);
    assert_eq!(consume(&wire).await.unwrap().failure_cause, None);
}

#[tokio::test]
async fn failure_details_follow_upstreams_capture_order() {
    let cases = [
        (
            r#"{"type":"error","message":"wafer route unavailable"}"#,
            "provider_error: wafer route unavailable",
        ),
        (
            r#"{"type":"error","code":"provider_down","message":"wafer route unavailable"}"#,
            "provider_down: wafer route unavailable",
        ),
        (
            r#"{"type":"error","error":{"type":"no_available_providers","message":"No providers are currently available"}}"#,
            "no_available_providers: No providers are currently available",
        ),
        (
            r#"{"type":"error","error":"plain text"}"#,
            "provider_error: plain text",
        ),
        (
            r#"{"type":"error","error":{"status":503}}"#,
            r#"{"status":503}"#,
        ),
        (
            r#"{"type":"error","providerError":{"reason":"overloaded"}}"#,
            "provider_error: overloaded",
        ),
        (r#"{"type":"error","providerError":7}"#, "7"),
        (r#"{"type":"error","code":"only_code"}"#, "only_code"),
    ];
    for (event, detail) in cases {
        let wire = events(&[
            event,
            r#"{"type":"finish","finishReason":{"unified":"error"}}"#,
        ]);
        let completion = consume(&wire).await.unwrap();
        assert_eq!(
            completion.failure_detail.as_deref(),
            Some(detail),
            "{event}"
        );
    }
}

#[tokio::test]
async fn the_first_failure_detail_wins() {
    let wire = events(&[
        r#"{"type":"error","error":{"code":"first","message":"one"}}"#,
        r#"{"type":"error","error":{"code":"second","message":"two"}}"#,
        r#"{"type":"finish","finishReason":{"unified":"error"},"error":{"code":"third","message":"three"}}"#,
    ]);
    let completion = consume(&wire).await.unwrap();
    assert_eq!(completion.failure_detail.as_deref(), Some("first: one"));
}

#[tokio::test]
async fn failure_details_are_masked_before_they_are_cut() {
    let message = format!("{}{KEY}", "x".repeat(590));
    let event = format!(r#"{{"type":"error","error":{{"code":"leak","message":"{message}"}}}}"#);
    let wire = events(&[
        &event,
        r#"{"type":"finish","finishReason":{"unified":"error"}}"#,
    ]);
    let detail = consume(&wire).await.unwrap().failure_detail.unwrap();
    assert!(!detail.contains("vck_live"), "{detail}");
    assert_eq!(detail.len(), MAX_FAILURE_DETAIL_BYTES);
    let multibyte = format!(r#"{{"type":"error","message":"{}"}}"#, "é".repeat(400));
    let wire = events(&[
        &multibyte,
        r#"{"type":"finish","finishReason":{"unified":"error"}}"#,
    ]);
    let detail = consume(&wire).await.unwrap().failure_detail.unwrap();
    assert!(detail.len() <= MAX_FAILURE_DETAIL_BYTES);
    assert!(detail.starts_with("provider_error: é"));
}

#[tokio::test]
async fn done_or_the_end_of_the_body_before_finish_leave_the_finish_empty() {
    let done = events(&[
        r#"{"type":"text-delta","id":"t1","delta":"partial"}"#,
        "[DONE]",
        r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
    ]);
    let completion = consume(&done).await.unwrap();
    assert_eq!(completion.content, "partial");
    assert_eq!(completion.finish, None);
    let cut = format!(
        "{}data: {{\"type\":\"finish\",\"finishReason\":{{\"unified\":\"stop\"}}}}",
        events(&[r#"{"type":"text-delta","delta":"partial"}"#])
    );
    let completion = consume(&cut).await.unwrap();
    assert_eq!(completion.content, "partial");
    assert_eq!(completion.finish, None);
}

#[tokio::test]
async fn text_and_reasoning_deltas_reach_the_sink_and_empty_ones_do_not() {
    let wire = events(&[
        r#"{"type":"start"}"#,
        r#"{"type":"reasoning-start","id":"r"}"#,
        r#"{"type":"reasoning-delta","id":"r","delta":"think"}"#,
        r#"{"type":"reasoning-delta","id":"r","delta":""}"#,
        r#"{"type":"text-delta","id":"t","delta":"Hello"}"#,
        r#"{"type":"text-delta","id":"t","delta":""}"#,
        r#"{"type":"text-delta","id":"t","delta":7}"#,
        r#"["not","an","object"]"#,
        r#"{"type":5}"#,
        r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
    ]);
    let (result, seen) = consume_wire(&wire, &CancellationToken::new()).await;
    assert_eq!(result.unwrap().content, "Hello");
    assert_eq!(
        seen,
        [
            StreamEvent::ReasoningDelta {
                text: "think".to_owned()
            },
            StreamEvent::TextDelta {
                text: "Hello".to_owned()
            },
        ]
    );
}

#[tokio::test]
async fn malformed_and_duplicate_key_events_end_the_stream() {
    for event in ["{not-json}", r#"{"type":"text-delta","type":"finish"}"#] {
        let wire = events(&[
            event,
            r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
        ]);
        let error = consume(&wire).await.unwrap_err();
        assert_eq!(error.code, "InvalidGatewaySseEvent");
        assert_eq!(
            error.detail.as_deref(),
            Some(format!("stream event 1 ({} bytes) was rejected", event.len()).as_str())
        );
    }
}

#[tokio::test]
async fn an_oversized_event_is_refused() {
    let wire = format!("data: \"{}\"\n\n", "a".repeat(MAX_SSE_EVENT_BYTES));
    let error = consume(&wire).await.unwrap_err();
    assert_eq!(error.code, "GatewaySseEventTooLarge");
}

#[tokio::test]
async fn a_read_failure_before_finish_is_a_transport_interruption() {
    let mut sink = |_: StreamEvent| {};
    let stream = Stream {
        requested_model: "test/model",
        secrets: &[],
    };
    let error = stream
        .consume(&mut Failing, &mut sink, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "ReadFailed");
    assert_eq!(error.kind, ProviderErrorKind::TransportInterrupted);
}

#[tokio::test]
async fn cancellation_stops_the_stream() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let wire = events(&[r#"{"type":"text-delta","delta":"never"}"#]);
    let (result, seen) = consume_wire(&wire, &cancel).await;
    assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Cancelled);
    assert!(seen.is_empty());
}
