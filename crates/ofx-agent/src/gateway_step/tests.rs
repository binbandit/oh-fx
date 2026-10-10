use ofx_contract::{ChatMessage, ProviderOptions, ToolCall, ToolChoice, Usage};
use ofx_trace::NetworkCallKind;

use super::*;
use crate::scripted_provider::{ScriptedProvider, calling, failure, text};

const CONTEXT: TraceContext = TraceContext {
    turn_id: 1,
    step_id: 2,
    subagent_id: 3,
};

fn ring() -> &'static NetworkRing {
    Box::leak(Box::new(NetworkRing::new()))
}

fn answered(kind: ProviderErrorKind, status: u16, detail: &str) -> ProviderError {
    ProviderError {
        status: Some(status),
        detail: Some(detail.to_owned()),
        ..failure(kind, "HttpFailure")
    }
}

fn request(messages: &[ChatMessage]) -> ModelRequest<'_> {
    ModelRequest {
        model: "reviewer",
        instructions: &[],
        messages,
        tools: &[],
        tool_choice: ToolChoice::None,
        max_output_tokens: None,
        provider_options: ProviderOptions::default(),
        session_id: None,
    }
}

#[test]
fn failures_settle_as_upstream_classifies_their_results() {
    let stream_failure = failure(ProviderErrorKind::RateLimited, "ProviderError");
    let truncated = failure(ProviderErrorKind::Protocol, "OutputTruncated");
    let filtered = failure(ProviderErrorKind::ProviderError, "ContentFiltered");
    let unfinished = failure(ProviderErrorKind::Protocol, "IncompleteStream");
    let not_found = answered(ProviderErrorKind::ProviderError, 404, "");
    let unavailable = answered(ProviderErrorKind::Unavailable, 503, "");
    let reset = failure(ProviderErrorKind::TransportInterrupted, "ReadFailed");
    assert_eq!(
        failure_settlement(&stream_failure),
        Settlement::Finished(ERROR)
    );
    assert_eq!(failure_settlement(&truncated), Settlement::Finished(LENGTH));
    assert_eq!(
        failure_settlement(&filtered),
        Settlement::Finished(CONTENT_FILTER)
    );
    assert_eq!(
        failure_settlement(&unfinished),
        Settlement::Finished(MISSING_FINISH)
    );
    assert_eq!(failure_settlement(&not_found), Settlement::Answered(520));
    assert_eq!(failure_settlement(&unavailable), Settlement::Answered(503));
    assert_eq!(failure_settlement(&reset), Settlement::Failed("ReadFailed"));
    assert_eq!(
        failure_settlement(&ProviderError::cancelled()),
        Settlement::Failed("Cancelled")
    );
}

#[test]
fn model_calls_keep_the_provider_stop_reason() {
    let ring = ring();
    let meter = Meter::new(ring, CONTEXT);
    meter.record("test/model", 0, &Ok(text("done")));
    meter.record(
        "test/model",
        0,
        &Err(failure(ProviderErrorKind::Protocol, "OutputTruncated")),
    );
    meter.record(
        "test/model",
        0,
        &Err(failure(ProviderErrorKind::Protocol, "IncompleteStream")),
    );
    meter.record(
        "test/model",
        0,
        &Ok(calling(ToolCall::new("call-1", "read_file", "{}"))),
    );
    let reasons: Vec<_> = ring
        .snapshot()
        .calls
        .iter()
        .map(|call| (call.status, call.stop_reason.clone()))
        .collect();
    assert_eq!(
        reasons,
        [
            (200, "stop".to_owned()),
            (200, "length".to_owned()),
            (200, String::new()),
            (200, "tool-calls".to_owned()),
        ]
    );
}

#[test]
fn model_calls_record_status_bytes_tokens_errors_and_ids() {
    let ring = ring();
    let meter = Meter::new(ring, CONTEXT);
    let started_at_ms = ofx_trace::timestamp_ms();
    meter.record(
        "test/model",
        started_at_ms,
        &Ok(Completion {
            usage: Usage {
                input_tokens: Some(1_200),
                output_tokens: Some(u64::MAX),
            },
            ..calling(ToolCall::new("call-1", "read_file", r#"{"path":"a"}"#))
        }),
    );
    meter.record(
        "test/model",
        started_at_ms,
        &Err(answered(
            ProviderErrorKind::Unavailable,
            503,
            "HTTP 503 · overloaded",
        )),
    );
    meter.record(
        "test/model",
        started_at_ms,
        &Err(failure(
            ProviderErrorKind::ConnectionFailed,
            "ConnectionFailed",
        )),
    );
    let calls = ring.snapshot().calls;
    assert_eq!(calls.len(), 3);
    for call in &calls {
        assert_eq!(call.kind, NetworkCallKind::Gateway);
        assert_eq!(call.model, "test/model");
        assert_eq!(call.started_at_ms, started_at_ms);
        assert_eq!((call.turn_id, call.step_id, call.subagent_id), (1, 2, 3));
    }
    assert_eq!(calls[0].status, 200);
    assert_eq!(calls[0].response_bytes, 6 + 9 + 12);
    assert_eq!(
        (calls[0].input_tokens, calls[0].output_tokens),
        (1_200, u32::MAX)
    );
    assert!(!calls[0].is_error());
    assert_eq!(calls[1].status, 503);
    assert_eq!(
        calls[1].response_bytes,
        u32::try_from("HTTP 503 · overloaded".len()).unwrap()
    );
    assert!(calls[1].is_error());
    assert_eq!(
        (calls[2].status, calls[2].error.as_str()),
        (0, "ConnectionFailed")
    );
    assert!(calls[2].is_error());
}

#[tokio::test]
async fn a_metered_provider_records_each_call_without_turn_ids() {
    let inner = Arc::new(ScriptedProvider::new(vec![
        Ok(text("A title")),
        Err(failure(ProviderErrorKind::Timeout, "Timeout")),
    ]));
    let ring = ring();
    let metered = MeteredProvider { inner, ring };
    let messages = [ChatMessage::user("name it")];
    let cancel = CancellationToken::new();
    let mut sink = |_| {};
    assert!(
        metered
            .stream(&request(&messages), &mut sink, &cancel)
            .await
            .is_ok()
    );
    assert!(
        metered
            .stream_body(&request(&messages), "{}".to_owned(), &mut sink, &cancel)
            .await
            .is_err()
    );
    let calls = ring.snapshot().calls;
    let shown: Vec<_> = calls
        .iter()
        .map(|call| {
            (
                call.model.as_str(),
                call.status,
                call.error.as_str(),
                (call.turn_id, call.step_id, call.subagent_id),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("reviewer", 200, "", (0, 0, 0)),
            ("reviewer", 0, "Timeout", (0, 0, 0)),
        ]
    );
}
