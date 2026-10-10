use std::time::Duration;

use ofx_contract::{ChatMessage, ProviderOptions, StreamEvent, ToolChoice, Usage};
use ofx_testkit::{FakeServer, RecordedRequest, RefusedPort, Reply};

use super::*;

const KEY: &str = "vck_live_0123456789abcdef";
const MODEL: &str = "openai/gpt-5.6-sol";
const USER_AGENT: &str = "oh-fx/test";
const STOP: &str = r#"{"type":"finish","finishReason":{"unified":"stop"},"usage":{"inputTokens":{"total":12},"outputTokens":{"total":3}}}"#;

fn provider(chat: String, secret: Option<&str>, team: Option<&str>) -> GatewayProvider {
    GatewayProvider::new(
        GatewayCredential::new(secret.map(str::to_owned), team.map(str::to_owned)),
        USER_AGENT,
        GatewayEndpoints { chat },
    )
    .unwrap()
}

fn chat_url(server: &FakeServer) -> String {
    format!("{}/v4/ai/language-model", server.base_url())
}

fn keyed(server: &FakeServer) -> GatewayProvider {
    provider(chat_url(server), Some(KEY), None)
}

fn request<'a>(messages: &'a [ChatMessage], session_id: Option<&'a str>) -> ModelRequest<'a> {
    ModelRequest {
        model: MODEL,
        instructions: &["Be concise."],
        messages,
        tools: &[],
        tool_choice: ToolChoice::Auto,
        max_output_tokens: Some(32_000),
        provider_options: ProviderOptions {
            prompt_caching: true,
            ..ProviderOptions::default()
        },
        session_id,
    }
}

async fn run(
    provider: &GatewayProvider,
    session_id: Option<&str>,
) -> (Result<Completion, ProviderError>, Vec<StreamEvent>) {
    let messages = [ChatMessage::user("question")];
    let request = request(&messages, session_id);
    let mut seen = Vec::new();
    let mut sink = |event: StreamEvent| seen.push(event);
    let result = provider
        .stream(&request, &mut sink, &CancellationToken::new())
        .await;
    (result, seen)
}

async fn finishing(events: &[&str]) -> Result<Completion, ProviderError> {
    let server = FakeServer::start([Reply::sse(events)]);
    run(&keyed(&server), None).await.0
}

fn only_request(server: &FakeServer) -> RecordedRequest {
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    requests.into_iter().next().unwrap()
}

#[tokio::test]
async fn a_turn_posts_upstreams_headers_and_body_and_streams_its_reply() {
    let server = FakeServer::start([Reply::sse(&[
        r#"{"type":"response-metadata","modelId":"openai/gpt-5.6-sol"}"#,
        r#"{"type":"reasoning-delta","id":"r","delta":"Thinking"}"#,
        r#"{"type":"text-delta","id":"t","delta":"Hello"}"#,
        r#"{"type":"text-delta","id":"t","delta":" there"}"#,
        STOP,
    ])]);
    let gateway = provider(chat_url(&server), Some(KEY), Some("team_123"));
    let (result, seen) = run(&gateway, Some("session_123")).await;
    let completion = result.unwrap();
    assert_eq!(completion.content.as_deref(), Some("Hello there"));
    assert_eq!(completion.finish_reason, FinishReason::Stop);
    assert!(completion.tool_calls.is_empty());
    assert_eq!(completion.provider_replay, None);
    assert_eq!(
        completion.usage,
        Usage {
            input_tokens: Some(12),
            output_tokens: Some(3),
        }
    );
    assert_eq!(
        seen,
        [
            StreamEvent::Admitted,
            StreamEvent::ReasoningDelta {
                text: "Thinking".to_owned()
            },
            StreamEvent::TextDelta {
                text: "Hello".to_owned()
            },
            StreamEvent::TextDelta {
                text: " there".to_owned()
            },
        ]
    );
    let sent = only_request(&server);
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/v1/v4/ai/language-model");
    for (name, value) in [
        ("content-type", "application/json"),
        ("user-agent", USER_AGENT),
        ("authorization", "Bearer vck_live_0123456789abcdef"),
        ("http-referer", "https://github.com/binbandit/oh-fx"),
        ("x-title", "oh-fx"),
        ("x-vercel-gateway-extended-time", "true"),
        ("ai-gateway-protocol-version", "0.0.1"),
        ("ai-language-model-specification-version", "4"),
        ("ai-language-model-id", MODEL),
        ("ai-language-model-streaming", "true"),
        ("x-vercel-ai-gateway-team", "team_123"),
        ("x-session-id", "session_123"),
        ("x-session-affinity", "session_123"),
    ] {
        assert_eq!(sent.header(name), Some(value), "{name}");
    }
    assert_eq!(sent.header("accept-encoding"), None);
    assert_eq!(
        sent.body_text(),
        r#"{"prompt":[{"role":"system","content":"Be concise."},{"role":"user","content":[{"type":"text","text":"question"}]}],"tools":[],"toolChoice":{"type":"auto"},"maxOutputTokens":32000,"providerOptions":{"gateway":{"caching":"auto"}}}"#
    );
}

#[tokio::test]
async fn without_a_key_team_or_session_those_headers_are_left_out() {
    let server = FakeServer::start([Reply::sse(&[STOP]), Reply::sse(&[STOP])]);
    let unauthenticated = provider(chat_url(&server), None, Some(""));
    run(&unauthenticated, Some("")).await.0.unwrap();
    let empty_key = provider(chat_url(&server), Some(""), None);
    run(&empty_key, None).await.0.unwrap();
    for sent in server.requests() {
        for name in [
            "authorization",
            "x-vercel-ai-gateway-team",
            "x-session-id",
            "x-session-affinity",
        ] {
            assert_eq!(sent.header(name), None, "{name}");
        }
    }
}

#[tokio::test]
async fn a_prepared_body_is_sent_unchanged() {
    let server = FakeServer::start([Reply::sse(&[STOP])]);
    let gateway = keyed(&server);
    let messages = [ChatMessage::user("question")];
    let request = request(&messages, None);
    let body = gateway.request_body(&request).unwrap();
    assert_eq!(body, build_request(&request, USER_AGENT).unwrap());
    let mut sink = |_: StreamEvent| {};
    gateway
        .stream_body(&request, body.clone(), &mut sink, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(only_request(&server).body_text(), body);
}

#[tokio::test]
async fn a_request_the_protocol_refuses_is_never_sent() {
    let server = FakeServer::start([]);
    let gateway = keyed(&server);
    let messages = [ChatMessage::System {
        content: "late".to_owned(),
    }];
    let request = request(&messages, None);
    assert_eq!(gateway.request_body(&request), None);
    let mut sink = |_: StreamEvent| {};
    let error = gateway
        .stream(&request, &mut sink, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ProviderError::new(ProviderErrorKind::Protocol, "InvalidProviderPrompt")
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn finish_reasons_map_onto_the_turns_completion_or_error() {
    let other = finishing(&[
        r#"{"type":"text-delta","delta":"done"}"#,
        r#"{"type":"finish","finishReason":{"unified":"other"}}"#,
    ])
    .await
    .unwrap();
    assert_eq!(other.finish_reason, FinishReason::Stop);
    assert_eq!(other.content.as_deref(), Some("done"));
    let empty = finishing(&[STOP]).await.unwrap();
    assert_eq!(empty.content, None);
    for (finish, kind, code) in [
        ("length", ProviderErrorKind::Protocol, "OutputTruncated"),
        (
            "content-filter",
            ProviderErrorKind::ProviderError,
            "ContentFiltered",
        ),
        (
            "tool-calls",
            ProviderErrorKind::Protocol,
            "InvalidProviderCompletion",
        ),
        ("error", ProviderErrorKind::ServerError, "ProviderError"),
    ] {
        let event = format!(r#"{{"type":"finish","finishReason":{{"unified":"{finish}"}}}}"#);
        let error = finishing(&[&event]).await.unwrap_err();
        assert_eq!((error.kind, error.code.as_str()), (kind, code), "{finish}");
    }
}

#[tokio::test]
async fn an_in_stream_provider_error_is_retryable_and_names_its_detail() {
    let error = finishing(&[
        r#"{"type":"error","error":{"code":"provider_down","message":"wafer route unavailable"}}"#,
        r#"{"type":"finish","finishReason":{"unified":"error"}}"#,
    ])
    .await
    .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::ServerError);
    assert_eq!(error.status, None);
    assert_eq!(
        error.detail.as_deref(),
        Some("provider error: provider_down: wafer route unavailable")
    );
    assert_eq!(
        error.diagnostic.as_deref(),
        Some("provider_down: wafer route unavailable")
    );
    let prose = finishing(&[
        r#"{"type":"error","message":"the route is down"}"#,
        r#"{"type":"finish","finishReason":{"unified":"error"}}"#,
    ])
    .await
    .unwrap_err();
    assert_eq!(
        prose.diagnostic.as_deref(),
        Some("provider_error: the route is down")
    );
    let bare = finishing(&[r#"{"type":"finish","finishReason":{"unified":"error"}}"#])
        .await
        .unwrap_err();
    assert_eq!(bare.diagnostic.as_deref(), Some("provider_error"));
    assert_eq!(
        bare.detail.as_deref(),
        Some("provider error: provider_error")
    );
    let filtered = finishing(&[r#"{"type":"finish","finishReason":{"unified":"content-filter"}}"#])
        .await
        .unwrap_err();
    assert_eq!(filtered.diagnostic.as_deref(), Some("content_filter"));
}

#[tokio::test]
async fn a_gateway_stream_timeout_is_a_stall_with_or_without_a_finish() {
    for events in [
        &[r#"{"type":"finish","finishReason":{"unified":"error","raw":"gateway_stream_timeout"}}"#]
            [..],
        &[r#"{"type":"error","error":{"code":"gateway_stream_timeout","message":"too long"}}"#][..],
    ] {
        let error = finishing(events).await.unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::StreamStalled, "{events:?}");
    }
    let unfinished = finishing(&[
        r#"{"type":"error","error":{"code":"gateway_stream_timeout","message":"too long"}}"#,
    ])
    .await
    .unwrap_err();
    assert_eq!(unfinished.code, "StreamInterrupted");
    assert_eq!(
        unfinished.diagnostic.as_deref(),
        Some("gateway_stream_timeout: too long")
    );
}

#[tokio::test]
async fn a_stream_that_ends_before_its_finish_is_interrupted() {
    for events in [
        &[r#"{"type":"text-delta","delta":"partial"}"#][..],
        &[r#"{"type":"text-delta","delta":"partial"}"#, "[DONE]"][..],
    ] {
        let error = finishing(events).await.unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::TransportInterrupted);
        assert_eq!(error.code, "StreamInterrupted");
        assert_eq!(
            error.detail.as_deref(),
            Some("the stream ended before its finish event after 1 events")
        );
        assert_eq!(error.diagnostic.as_deref(), Some("StreamInterrupted"));
    }
}

#[tokio::test]
async fn http_failures_keep_their_status_kind_retry_after_and_masked_detail() {
    let server = FakeServer::start([
        Reply::status(
            401,
            format!(
                r#"{{"error":{{"code":"invalid_api_key","message":"key {KEY} is not valid"}}}}"#
            ),
        ),
        Reply::status_with_headers(
            429,
            &[("retry-after", "7")],
            r#"{"error":{"message":"slow down"}}"#,
        ),
        Reply::status(503, "unavailable"),
    ]);
    let gateway = keyed(&server);
    let unauthorized = run(&gateway, None).await.0.unwrap_err();
    assert_eq!(unauthorized.kind, ProviderErrorKind::Unauthorized);
    assert_eq!(unauthorized.status, Some(401));
    let shown = unauthorized.detail.unwrap();
    assert!(
        shown.starts_with("API access denied · HTTP 401 · invalid_api_key"),
        "{shown}"
    );
    assert!(!shown.contains("vck_live"), "{shown}");
    let limited = run(&gateway, None).await.0.unwrap_err();
    assert_eq!(limited.kind, ProviderErrorKind::RateLimited);
    assert_eq!(limited.retry_after, Some(Duration::from_secs(7)));
    let unavailable = run(&gateway, None).await.0.unwrap_err();
    assert_eq!(unavailable.kind, ProviderErrorKind::Unavailable);
    assert_eq!(unavailable.status, Some(503));
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn an_unreachable_gateway_fails_as_a_connection_failure() {
    let port = RefusedPort::reserve();
    let gateway = provider(
        format!("{}/language-model", port.base_url()),
        Some(KEY),
        None,
    );
    let error = run(&gateway, None).await.0.unwrap_err();
    assert_eq!(error.code, "ConnectionFailed");
    assert!(!error.detail.unwrap_or_default().contains("vck_live"));
}

#[tokio::test]
async fn a_cancelled_turn_sends_nothing() {
    let server = FakeServer::start([]);
    let gateway = keyed(&server);
    let messages = [ChatMessage::user("question")];
    let request = request(&messages, None);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut sink = |_: StreamEvent| {};
    let error = gateway
        .stream(&request, &mut sink, &cancel)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Cancelled);
    assert!(server.requests().is_empty());
}

#[test]
fn debug_output_never_shows_the_key() {
    let credential = GatewayCredential::new(Some(KEY.to_owned()), Some("team".to_owned()));
    let shown = format!("{credential:?}");
    assert!(!shown.contains("vck_live"), "{shown}");
    assert!(shown.contains("<redacted>"));
    let gateway = provider("http://127.0.0.1:9/chat".to_owned(), Some(KEY), None);
    assert!(!format!("{gateway:?}").contains("vck_live"));
    assert_eq!(GatewayEndpoints::default().chat, CHAT_URL);
}

#[test]
fn diagnostics_keep_identified_details_and_name_the_cause_otherwise() {
    assert_eq!(diagnostic(None, "provider_error"), "provider_error");
    assert_eq!(diagnostic(Some("  "), "provider_error"), "provider_error");
    assert_eq!(
        diagnostic(Some("code: message"), "provider_error"),
        "code: message"
    );
    assert_eq!(
        diagnostic(Some("rate_limited"), "provider_error"),
        "rate_limited"
    );
    assert_eq!(
        diagnostic(Some("two words: message"), "provider_error"),
        "provider_error: two words: message"
    );
    assert_eq!(diagnostic(Some(": x"), "fallback"), "fallback: : x");
}
