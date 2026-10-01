use std::collections::VecDeque;

use ofx_config::{MaxTokensParameter, ToolChoiceMode};
use ofx_contract::{ChatMessage, FinishReason, ToolChoice, ToolSpec};
use ofx_testkit::{FakeServer, Reply, chat_text_events};

use super::*;
use crate::chat_completions_protocol::Selection;

const TEST_STOP: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
const TEST_TEXT: &str = r#"{"id":"chat-1","model":"resolved-model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}"#;
const PORTKEY_KEY: &str = "pk-live-secret";

struct OwnedRequest {
    instructions: Vec<&'static str>,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolSpec>,
    max_output_tokens: Option<u32>,
}

impl OwnedRequest {
    fn borrowed(&self) -> ModelRequest<'_> {
        ModelRequest {
            model: "opaque/local-model:8b",
            instructions: &self.instructions,
            messages: &self.messages,
            tools: &self.tools,
            tool_choice: ToolChoice::Auto,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

fn test_request() -> OwnedRequest {
    OwnedRequest {
        instructions: vec!["first", "second"],
        messages: vec![ChatMessage::user("hi")],
        tools: Vec::new(),
        max_output_tokens: None,
    }
}

fn tool_request() -> OwnedRequest {
    OwnedRequest {
        tools: vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file.".to_owned(),
            input_schema: serde_json::json!({"type":"object"}),
        }],
        ..test_request()
    }
}

fn selection(request: &OwnedRequest) -> Selection {
    let options = RequestOptions {
        tool_choice_mode: ToolChoiceMode::Omit,
        max_tokens_parameter: MaxTokensParameter::MaxTokens,
    };
    build_request(&request.borrowed(), options)
        .unwrap()
        .selection
}

struct Chunks {
    chunks: VecDeque<Vec<u8>>,
}

impl Chunks {
    fn split(wire: &[u8], size: usize) -> Self {
        Self {
            chunks: wire.chunks(size.max(1)).map(<[u8]>::to_vec).collect(),
        }
    }
}

impl ChunkSource for Chunks {
    async fn next_chunk(&mut self) -> Result<Option<impl AsRef<[u8]> + Send>, String> {
        Ok(self.chunks.pop_front())
    }
}

async fn consume_request(
    request: &OwnedRequest,
    wire: &[u8],
    size: usize,
    limits: Limits,
    sink: &mut dyn StreamSink,
    cancel: &CancellationToken,
) -> Result<Completion, ProviderError> {
    let mut reducer = Reducer::new(selection(request), limits);
    let secrets = [PORTKEY_KEY.to_owned()];
    Stream {
        limits,
        secrets: &secrets,
    }
    .consume(&mut Chunks::split(wire, size), &mut reducer, sink, cancel)
    .await
}

async fn consume_wire(
    wire: &[u8],
    size: usize,
    limits: Limits,
    sink: &mut dyn StreamSink,
    cancel: &CancellationToken,
) -> Result<Completion, ProviderError> {
    consume_request(&test_request(), wire, size, limits, sink, cancel).await
}

async fn consume_simple(wire: &[u8], limits: Limits) -> Result<Completion, ProviderError> {
    consume_wire(
        wire,
        wire.len(),
        limits,
        &mut |_| {},
        &CancellationToken::new(),
    )
    .await
}

fn sse(events: &[&str]) -> Vec<u8> {
    events
        .iter()
        .flat_map(|event| ["data: ", event, "\n\n"])
        .collect::<String>()
        .into_bytes()
}

#[tokio::test]
async fn chat_completions_stream_framing_handles_chunks_trailers_truncation_and_wire_caps() {
    let wire = sse(&[
        TEST_TEXT,
        TEST_STOP,
        r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":1}}"#,
        "[DONE]",
    ]);
    for size in [1, 2, 7, 31, 1024] {
        let completion = consume_wire(
            &wire,
            size,
            Limits::default(),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(completion.content.as_deref(), Some("hello"));
        assert_eq!(completion.usage.input_tokens, Some(5));
    }
    for truncated in [
        String::new(),
        "data: [DONE]\n\n".to_owned(),
        format!("data: {TEST_TEXT}\n\n"),
    ] {
        let error = consume_simple(truncated.as_bytes(), Limits::default())
            .await
            .unwrap_err();
        assert_eq!(error.code, "IncompleteStream", "{truncated}");
    }
    let comments = ": comment\n".repeat(20);
    let limits = Limits {
        event_bytes: 30,
        ..Limits::default()
    };
    assert_eq!(
        consume_simple(comments.as_bytes(), limits)
            .await
            .unwrap_err()
            .code,
        "EventTooLarge"
    );
    let comment_events = ": comment\n\n".repeat(20);
    let limits = Limits {
        total_wire_bytes: 30,
        ..Limits::default()
    };
    assert_eq!(
        consume_simple(comment_events.as_bytes(), limits)
            .await
            .unwrap_err()
            .code,
        "StreamTooLarge"
    );
    let limits = Limits {
        total_wire_bytes: wire.len() - 1,
        ..Limits::default()
    };
    assert_eq!(
        consume_simple(&wire, limits).await.unwrap_err().code,
        "StreamTooLarge"
    );
}

#[tokio::test]
async fn chat_completions_accepts_a_missing_done_marker_after_a_finish() {
    for wire in [
        format!("data: {TEST_TEXT}\n\ndata: {TEST_STOP}\n\n"),
        format!("data: {TEST_TEXT}\n\ndata: {TEST_STOP}\n\ndata: [DONE]"),
        format!("data: {TEST_TEXT}\n\ndata: {TEST_STOP}\n\ndata: [DONE]\n"),
    ] {
        let completion = consume_simple(wire.as_bytes(), Limits::default())
            .await
            .unwrap();
        assert_eq!(completion.content.as_deref(), Some("hello"));
        assert_eq!(completion.finish_reason, FinishReason::Stop);
    }
}

#[tokio::test]
async fn chat_completions_reasoning_presentation_checks_cancellation_before_content_from_the_same_event()
 {
    let wire = sse(&[
        r#"{"choices":[{"index":0,"delta":{"reasoning":"think\n","content":"answer"}}]}"#,
        TEST_STOP,
        "[DONE]",
    ]);
    for cancel_on_reasoning in [false, true] {
        let cancel = CancellationToken::new();
        let observer = cancel.clone();
        let mut reasoning = 0;
        let mut content = 0;
        let mut sink = |event: StreamEvent| match event {
            StreamEvent::ReasoningDelta { text } => {
                assert_eq!(text, "think\n");
                reasoning += 1;
                if cancel_on_reasoning {
                    observer.cancel();
                }
            }
            StreamEvent::TextDelta { .. } => content += 1,
        };
        let outcome = consume_wire(&wire, wire.len(), Limits::default(), &mut sink, &cancel).await;
        if cancel_on_reasoning {
            assert_eq!(outcome.unwrap_err().kind, ProviderErrorKind::Cancelled);
        } else {
            assert!(outcome.is_ok());
        }
        assert_eq!(reasoning, 1);
        assert_eq!(content, usize::from(!cancel_on_reasoning));
    }
}

#[tokio::test]
async fn chat_completions_cancellation_stops_consumption_and_progress_cannot_complete() {
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let wire = sse(&[TEST_TEXT]);
    let error = consume_wire(
        &wire,
        wire.len(),
        Limits::default(),
        &mut |_| {},
        &cancelled,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Cancelled);
    let cancel = CancellationToken::new();
    let observer = cancel.clone();
    let mut count = 0;
    let mut sink = |event: StreamEvent| {
        if matches!(event, StreamEvent::TextDelta { .. }) {
            count += 1;
        }
        observer.cancel();
    };
    let wire = sse(&[TEST_TEXT, TEST_STOP, "[DONE]"]);
    let error = consume_wire(&wire, wire.len(), Limits::default(), &mut sink, &cancel)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Cancelled);
    assert_eq!(count, 1);
}

#[tokio::test]
async fn chat_completions_exact_wire_bound_includes_the_terminal_marker() {
    let wire = sse(&[TEST_TEXT, TEST_STOP, "[DONE]"]);
    let limits = Limits {
        total_wire_bytes: wire.len(),
        ..Limits::default()
    };
    assert!(consume_simple(&wire, limits).await.is_ok());
}

fn framed(events: &[&str], eol: &str) -> Vec<u8> {
    let framed_events = events
        .iter()
        .map(|event| format!("event: message{eol}data: {event}{eol}: ping{eol}{eol}"));
    std::iter::once(format!("\u{feff}: keepalive{eol}{eol}"))
        .chain(framed_events)
        .collect::<String>()
        .into_bytes()
}

#[tokio::test]
async fn every_split_of_crlf_cr_and_lf_framing_preserves_utf8_text_and_tool_arguments() {
    let events = [
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"héllo 🌍 "},"finish_reason":null}]}"#,
        r#"{"id":"c","choices":[{"index":0,"delta":{"content":"é🌍"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read_"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"th\":\"a\\"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"b🌍\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
        "[DONE]",
    ];
    let request = tool_request();
    for eol in ["\n", "\r\n", "\r"] {
        let wire = framed(&events, eol);
        for size in 1..=wire.len().min(97) {
            let mut text = String::new();
            let mut sink = |event: StreamEvent| {
                if let StreamEvent::TextDelta { text: delta } = event {
                    text.push_str(&delta);
                }
            };
            let completion = consume_request(
                &request,
                &wire,
                size,
                Limits::default(),
                &mut sink,
                &CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("eol {eol:?} size {size}: {error:?}"));
            assert_eq!(text, "héllo 🌍 é🌍");
            assert_eq!(
                completion.tool_calls[0].arguments,
                "{\"path\":\"a\\\"b🌍\"}"
            );
        }
    }
}

#[tokio::test]
async fn protocol_failures_report_rejected_data_by_position_and_size() {
    let rejected = format!("{{not json {PORTKEY_KEY} \u{1b}[31m");
    let wire = sse(&[TEST_TEXT, &rejected]);
    let error = consume_simple(&wire, Limits::default()).await.unwrap_err();
    assert_eq!(error.code, "InvalidChunk");
    assert_eq!(
        error.detail,
        Some(format!(
            "stream event 2 ({} bytes) was rejected",
            rejected.len()
        ))
    );
    let wire = sse(&[
        TEST_TEXT,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    let error = consume_simple(&wire, Limits::default()).await.unwrap_err();
    assert_eq!(error.code, "InconsistentFinishReason");
    assert_eq!(
        error.detail.as_deref(),
        Some("finish_reason tool_calls arrived without any tool calls")
    );
    let error = consume_simple(b"data: {\"choices\":[]}\n\n", Limits::default())
        .await
        .unwrap_err();
    assert_eq!(error.code, "IncompleteStream");
    assert_eq!(
        error.detail.as_deref(),
        Some("the stream ended before a finish_reason after 1 events and 22 bytes")
    );
}

fn secret_across_byte_160(lead: &str) -> String {
    format!("{lead}{}{PORTKEY_KEY} tail", "p".repeat(150 - lead.len()))
}

#[tokio::test]
async fn a_secret_across_the_old_excerpt_cut_never_reaches_a_stream_failure() {
    let rejected = secret_across_byte_160("{not json ");
    let wire = sse(&[TEST_TEXT, &rejected]);
    let error = consume_simple(&wire, Limits::default()).await.unwrap_err();
    let detail = error.detail.unwrap();
    assert_eq!(
        detail,
        format!("stream event 2 ({} bytes) was rejected", rejected.len())
    );
    assert!(!detail.contains(&PORTKEY_KEY[..10]), "{detail}");
    let wire = format!(
        "{}\n\ndata: {{\"choices\":[]}}\n\n",
        secret_across_byte_160(": ")
    );
    for size in [1, 7, wire.len()] {
        let error = consume_wire(
            wire.as_bytes(),
            size,
            Limits::default(),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        let detail = error.detail.unwrap();
        assert_eq!(
            detail,
            format!(
                "the stream ended before a finish_reason after 1 events and {} bytes",
                wire.len()
            )
        );
        assert!(!detail.contains(&PORTKEY_KEY[..10]), "{detail}");
    }
}

#[tokio::test]
async fn in_stream_provider_errors_are_masked_and_terminal_safe() {
    for event in [
        r#"{"error":{"message":"invalid x-portkey-api-key pk-live-secret \u202e\u001b[31mred","code":"401"}}"#,
        r#"{"object":"error","message":"context too long for pk-live-secret","type":"BadRequestError","code":400}"#,
    ] {
        let wire = sse(&[TEST_TEXT, event]);
        let error = consume_simple(&wire, Limits::default()).await.unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::ProviderError);
        assert_eq!(error.code, "ProviderError");
        let detail = error.detail.unwrap();
        assert!(detail.starts_with("provider error: {"), "{detail}");
        assert!(!detail.contains(PORTKEY_KEY), "{detail}");
        assert!(
            !detail.contains('\u{1b}') && !detail.contains('\u{202e}'),
            "{detail}"
        );
    }
}

#[tokio::test]
async fn an_error_finish_reason_is_classified_as_a_provider_error() {
    let wire = sse(&[
        TEST_TEXT,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"error"}]}"#,
    ]);
    let error = consume_simple(&wire, Limits::default()).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::ProviderError);
    assert_eq!(error.code, "ProviderError");
}

fn connection(
    base_url: &str,
    bearer: Option<&str>,
    headers: &[(&str, &str)],
) -> ResolvedConnection {
    ResolvedConnection {
        chat_url: format!("{base_url}/chat/completions"),
        bearer_token: bearer.map(str::to_owned),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        secrets: bearer
            .into_iter()
            .chain([PORTKEY_KEY])
            .map(str::to_owned)
            .collect(),
        tool_choice_mode: ToolChoiceMode::Omit,
        max_tokens_parameter: MaxTokensParameter::MaxTokens,
        ca_file: None,
        proxy: None,
    }
}

fn portkey(server: &FakeServer) -> ChatCompletionsProvider {
    portkey_at(&server.base_url())
}

fn portkey_at(base_url: &str) -> ChatCompletionsProvider {
    let connection = connection(
        base_url,
        None,
        &[
            ("x-portkey-api-key", PORTKEY_KEY),
            ("x-portkey-provider", "openai"),
        ],
    );
    ChatCompletionsProvider::new(connection, "oh-fx/test").unwrap()
}

async fn stream_text(
    provider: &ChatCompletionsProvider,
    request: &OwnedRequest,
) -> (Result<Completion, ProviderError>, String) {
    let mut text = String::new();
    let mut sink = |event: StreamEvent| {
        if let StreamEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    };
    let outcome = provider
        .stream(&request.borrowed(), &mut sink, &CancellationToken::new())
        .await;
    (outcome, text)
}

#[tokio::test]
async fn portkey_connections_send_configured_headers_and_the_exact_body() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Hel", "lo"]))]);
    let provider = portkey(&server);
    let (outcome, text) = stream_text(&provider, &test_request()).await;
    let completion = outcome.unwrap();
    assert_eq!(text, "Hello");
    assert_eq!(completion.content.as_deref(), Some("Hello"));
    assert_eq!(completion.usage.input_tokens, Some(12));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.header("x-portkey-api-key"), Some(PORTKEY_KEY));
    assert_eq!(request.header("x-portkey-provider"), Some("openai"));
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("user-agent"), Some("oh-fx/test"));
    assert_eq!(request.header("authorization"), None);
    assert_eq!(
        request.body_text(),
        r#"{"model":"opaque/local-model:8b","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"system","content":"first"},{"role":"system","content":"second"},{"role":"user","content":"hi"}]}"#
    );
}

#[tokio::test]
async fn bearer_connections_send_an_authorization_header() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["ok"]))]);
    let connection = connection(&server.base_url(), Some("sk-router-secret"), &[]);
    let provider = ChatCompletionsProvider::new(connection, "oh-fx/test").unwrap();
    let (outcome, _) = stream_text(&provider, &test_request()).await;
    assert!(outcome.is_ok());
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some("Bearer sk-router-secret")
    );
}

#[tokio::test]
async fn http_failures_map_status_retry_after_and_mask_details() {
    let body = format!(
        r#"{{"error":{{"message":"bad key {PORTKEY_KEY} for openai \u001b[2J","code":"invalid_api_key"}}}}"#
    );
    let server = FakeServer::start([
        Reply::status(401, body),
        Reply::status_with_headers(
            429,
            &[("Retry-After", " 7 ")],
            r#"{"error":{"message":"slow down"}}"#,
        ),
        Reply::status_with_headers(404, &[("Content-Type", "text/html")], "<html>\nnope</html>"),
    ]);
    let provider = portkey(&server);
    let (outcome, _) = stream_text(&provider, &test_request()).await;
    let error = outcome.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
    assert_eq!(error.status, Some(401));
    assert_eq!(error.code, "unauthorized");
    let detail = error.detail.unwrap();
    assert!(
        detail
            .starts_with("API access denied · HTTP 401 · invalid_api_key: bad key **************"),
        "{detail}"
    );
    assert!(detail.contains("for openai"), "{detail}");
    assert!(detail.ends_with("\\x1b[2J"), "{detail}");
    let (outcome, _) = stream_text(&provider, &test_request()).await;
    let error = outcome.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
    assert_eq!(error.diagnostic.as_deref(), Some("HTTP 429 · slow down"));
    assert_eq!(
        error.detail.as_deref(),
        Some("API request failed · HTTP 429 · slow down")
    );
    let (outcome, _) = stream_text(&provider, &test_request()).await;
    let error = outcome.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::ProviderError);
    assert_eq!(error.status, Some(404));
    assert_eq!(
        error.detail.as_deref(),
        Some("HTTP 404: <html>\\x0anope</html>")
    );
}

#[tokio::test]
async fn error_bodies_mask_encoded_echoes_of_the_key() {
    let plain = [
        PORTKEY_KEY,
        "%70%6B%2D%6C%69%76%65%2D%73%65%63%72%65%74",
        "\\u0070\\u006b-live-secret",
        "%5Cu0070k-live-secret",
    ];
    let nested = [
        "\\\\u0025\\\\u0037\\\\u0030k-live-secret",
        "%5Cu0070k-live-secret",
    ];
    let server = FakeServer::start([
        Reply::status_with_headers(
            500,
            &[("Content-Type", "text/plain")],
            format!("upstream rejected {}", plain.join(", ")),
        ),
        Reply::status(
            500,
            format!(
                r#"{{"error":{{"message":"upstream rejected {}"}}}}"#,
                nested.join(", ")
            ),
        ),
    ]);
    let provider = portkey(&server);
    for (separator, echoes) in [(": ", &plain[..]), (" · ", &nested[..])] {
        let (outcome, _) = stream_text(&provider, &test_request()).await;
        let error = outcome.unwrap_err();
        let masked: Vec<String> = echoes.iter().map(|echo| "*".repeat(echo.len())).collect();
        let expected = format!("HTTP 500{separator}upstream rejected {}", masked.join(", "));
        assert_eq!(error.diagnostic.as_deref(), Some(expected.as_str()));
        let detail = error.detail.unwrap();
        assert!(detail.ends_with(&expected), "{detail}");
    }
}

#[tokio::test]
async fn an_error_body_cut_off_mid_secret_fails_as_read_failed_without_its_bytes() {
    let body = format!("upstream rejected {}", &PORTKEY_KEY[..10]);
    let server = FakeServer::start([Reply::cut_off(500, body.as_str())]);
    let (outcome, _) = stream_text(&portkey(&server), &test_request()).await;
    let error = outcome.unwrap_err();
    assert_eq!(
        error.detail,
        Some(format!(
            "the HTTP 500 response body failed with UnexpectedEof after {} bytes",
            body.len()
        ))
    );
    assert_eq!(error.diagnostic, None);
    assert_eq!(error.kind, ProviderErrorKind::TransportInterrupted);
    assert_eq!(error.code, "ReadFailed");
    assert_eq!(error.status, None);
}

#[tokio::test]
async fn redirects_are_reported_and_never_followed_with_secret_headers() {
    for status in [301, 302, 307, 308] {
        let elsewhere = FakeServer::start([Reply::sse(&chat_text_events(&["owned"]))]);
        let location = format!(
            "{}/authorize?client_id=x&state=s3cret",
            elsewhere.base_url()
        );
        let gateway = FakeServer::start([Reply::status_with_headers(
            status,
            &[("Location", &location)],
            "",
        )]);
        let provider = portkey(&gateway);
        let (outcome, text) = stream_text(&provider, &test_request()).await;
        let error = outcome.unwrap_err();
        assert!(text.is_empty());
        assert!(elsewhere.requests().is_empty(), "{status} was followed");
        assert_eq!(error.status, Some(status));
        assert_eq!(error.kind, ProviderErrorKind::ProviderError);
        let detail = error.detail.unwrap();
        let host = elsewhere.base_url().trim_end_matches("/v1").to_owned();
        assert_eq!(
            detail,
            format!(
                "HTTP {status}: redirect to {host} was not followed; base_url must point at the gateway API itself, not at a sign-in page or a proxy that redirects"
            )
        );
        assert!(!detail.contains("s3cret"));
    }
}

#[tokio::test]
async fn redirects_are_reported_without_waiting_for_their_body() {
    let gateway = FakeServer::start([Reply::held_status_with_headers(
        302,
        &[("Location", "https://sso.example.com/login?state=s3cret")],
        "<html>Redirecting",
    )]);
    let provider = portkey(&gateway);
    let (outcome, _) = tokio::time::timeout(
        Duration::from_secs(5),
        stream_text(&provider, &test_request()),
    )
    .await
    .expect("the redirect is reported before its body ends");
    let error = outcome.unwrap_err();
    assert_eq!(error.status, Some(302));
    assert_eq!(
        error.detail.as_deref(),
        Some(
            "HTTP 302: redirect to https://sso.example.com was not followed; base_url must point at the gateway API itself, not at a sign-in page or a proxy that redirects"
        )
    );
}

#[tokio::test]
async fn non_event_stream_success_bodies_are_reported_by_media_type_and_size() {
    let completion = r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;
    let page = "<html><title>Sign in to Corp SSO</title></html>";
    let echo = secret_across_byte_160("<html>");
    let html = [("Content-Type", "text/html; charset=utf-8")];
    let server = FakeServer::start([
        Reply::status(200, completion),
        Reply::status_with_headers(200, &html, page),
        Reply::status_with_headers(200, &html, echo.as_str()),
        Reply::status_with_headers(200, &html, "x".repeat(MAX_ERROR_BODY_BYTES + 1)),
        Reply::status(
            200,
            r#"{"error":{"message":"quota exhausted for pk-live-secret"}}"#,
        ),
    ]);
    let provider = portkey(&server);
    let expected = [
        format!("application/json ({} bytes)", completion.len()),
        format!("text/html ({} bytes)", page.len()),
        format!("text/html ({} bytes)", echo.len()),
        format!("text/html (more than {MAX_ERROR_BODY_BYTES} bytes)"),
    ];
    for sent in expected {
        let (outcome, _) = stream_text(&provider, &test_request()).await;
        let error = outcome.unwrap_err();
        assert_eq!(error.code, "UnexpectedContentType");
        let detail = error.detail.unwrap();
        assert_eq!(
            detail,
            format!("expected text/event-stream but the provider sent {sent}")
        );
        assert!(!detail.contains(&PORTKEY_KEY[..10]), "{detail}");
    }
    let (outcome, _) = stream_text(&provider, &test_request()).await;
    let error = outcome.unwrap_err();
    assert_eq!(error.code, "ProviderError");
    let detail = error.detail.unwrap();
    assert!(detail.starts_with("provider error: "), "{detail}");
    assert!(!detail.contains(PORTKEY_KEY), "{detail}");
}

#[tokio::test]
async fn cancellation_interrupts_an_open_stream() {
    let server = FakeServer::start([Reply::held_sse(&[TEST_TEXT])]);
    let provider = portkey(&server);
    let cancel = CancellationToken::new();
    let observer = cancel.clone();
    let mut sink = move |_: StreamEvent| observer.cancel();
    let request = test_request();
    let error = provider
        .stream(&request.borrowed(), &mut sink, &cancel)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Cancelled);
}

#[tokio::test]
async fn transport_failures_are_classified_for_recovery() {
    let reserved = tokio::net::TcpSocket::new_v4().unwrap();
    reserved.bind(([127, 0, 0, 1], 0).into()).unwrap();
    let address = reserved.local_addr().unwrap();
    let provider = portkey_at(&format!("http://{address}/v1"));
    let (outcome, _) = stream_text(&provider, &test_request()).await;
    let error = outcome.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::ConnectivityLost);
    assert_eq!(error.code, "ConnectionFailed");
    assert!(error.detail.is_some());
    let server = FakeServer::start([Reply::Disconnect]);
    let (outcome, _) = stream_text(&portkey(&server), &test_request()).await;
    let error = outcome.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::TransportInterrupted);
    assert_eq!(error.code, "RequestFailed");
}

#[tokio::test]
async fn invalid_requests_fail_before_any_delivery() {
    let server = FakeServer::start([]);
    let provider = portkey(&server);
    let mut request = test_request();
    request.messages = vec![ChatMessage::System {
        content: "untrusted".to_owned(),
    }];
    let (outcome, _) = stream_text(&provider, &request).await;
    let error = outcome.unwrap_err();
    assert_eq!(error.code, "InvalidProviderPrompt");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn tool_choice_and_output_limit_options_reach_the_wire() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["ok"])),
        Reply::sse(&chat_text_events(&["ok"])),
    ]);
    let mut send = connection(&server.base_url(), None, &[]);
    send.tool_choice_mode = ToolChoiceMode::Send;
    let mut request = tool_request();
    request.max_output_tokens = Some(512);
    let (outcome, _) = stream_text(
        &ChatCompletionsProvider::new(send, "oh-fx/test").unwrap(),
        &request,
    )
    .await;
    assert!(outcome.is_ok());
    let mut completion_tokens = connection(&server.base_url(), None, &[]);
    completion_tokens.max_tokens_parameter = MaxTokensParameter::MaxCompletionTokens;
    let (outcome, _) = stream_text(
        &ChatCompletionsProvider::new(completion_tokens, "oh-fx/test").unwrap(),
        &request,
    )
    .await;
    assert!(outcome.is_ok());
    let requests = server.requests();
    let first = requests[0].json();
    assert_eq!(first["tool_choice"], "auto");
    assert_eq!(first["max_tokens"], 512);
    assert!(first.get("max_completion_tokens").is_none());
    let second = requests[1].json();
    assert!(second.get("tool_choice").is_none());
    assert!(second.get("max_tokens").is_none());
    assert_eq!(second["max_completion_tokens"], 512);
}
