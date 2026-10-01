use std::collections::VecDeque;
use std::fmt::Write;

use ofx_contract::{ToolCall, ToolCallId, ToolChoice, ToolResultStatus, ToolSpec};
use ofx_testkit::{FakeServer, Reply};
use serde_json::json;

use super::*;

struct Chunks(VecDeque<Vec<u8>>);

impl ChunkSource for Chunks {
    async fn next_chunk(&mut self) -> Result<Option<impl AsRef<[u8]> + Send>, String> {
        Ok(self.0.pop_front())
    }
}

fn request<'a>(
    messages: &'a [ChatMessage],
    instructions: &'a [&'a str],
    tools: &'a [ToolSpec],
) -> ModelRequest<'a> {
    ModelRequest {
        model: "gpt-5.6-sol",
        instructions,
        messages,
        tools,
        tool_choice: ToolChoice::Auto,
        max_output_tokens: Some(1024),
    }
}

fn build(messages: &[ChatMessage], replays: &[Option<&str>]) -> Result<String, ProviderError> {
    build_request(&request(messages, &[], &[]), replays).map_err(codex_failure)
}

fn assistant_calls(calls: Vec<ToolCall>) -> ChatMessage {
    ChatMessage::Assistant {
        content: None,
        tool_calls: calls,
        provider_replay: None,
    }
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn sized_state(size: usize) -> String {
    let prefix = r#"[{"type":"reasoning","encrypted_content":""#;
    let suffix = r#""}]"#;
    format!(
        "{prefix}{}{suffix}",
        "x".repeat(size - prefix.len() - suffix.len())
    )
}

fn sized_arguments(size: usize) -> String {
    let prefix = r#"{"value":""#;
    let suffix = r#""}"#;
    format!(
        "{prefix}{}{suffix}",
        "x".repeat(size - prefix.len() - suffix.len())
    )
}

async fn consume(sse: &str, limits: StreamLimits) -> Result<ResponsesCompletion, ProviderError> {
    let mut chunks = Chunks(VecDeque::from([sse.as_bytes().to_vec()]));
    let mut events = Vec::new();
    let mut sink = |event: StreamEvent| events.push(event);
    consume_stream(
        &mut chunks,
        &mut sink,
        &CancellationToken::new(),
        limits,
        &[],
    )
    .await
}

async fn expect_sse_error(code: &str, sse: &str, limits: StreamLimits) {
    assert_eq!(consume(sse, limits).await.unwrap_err().code, code, "{sse}");
}

#[test]
fn openai_codex_request_uses_responses_input_and_converts_ai_sdk_tool_schemas() {
    let tools = [ToolSpec {
        name: "read_file".to_owned(),
        description: "Read".to_owned(),
        input_schema: json!({"type": "object", "properties": {}}),
    }];
    let messages = [
        ChatMessage::user("Read it."),
        assistant_calls(vec![call("call_1", "read_file", r#"{"path":"README.md"}"#)]),
        ChatMessage::Tool {
            call_id: ToolCallId::new("call_1"),
            tool_name: "read_file".to_owned(),
            content: "contents".to_owned(),
            status: ToolResultStatus::Success,
        },
    ];
    let body = build_request(
        &request(&messages, &["Be concise."], &tools),
        &[
            None,
            Some(r#"[{"id":"rs_1","type":"reasoning","encrypted_content":"opaque"}]"#),
            None,
        ],
    )
    .unwrap();
    assert!(body.contains(r#""model":"gpt-5.6-sol""#));
    assert!(body.contains(r#""instructions":"Be concise.""#));
    assert!(body.contains(r#""type":"function_call_output""#));
    assert!(body.contains(r#""encrypted_content":"opaque""#));
    assert!(body.contains(r#""parameters":{"type":"object","properties":{}}"#));
    assert!(!body.contains("max_output_tokens"));
}

#[test]
fn openai_codex_replay_provider_state_accepts_the_limit_and_rejects_one_byte_beyond() {
    let message = [ChatMessage::Assistant {
        content: Some("a".to_owned()),
        tool_calls: Vec::new(),
        provider_replay: None,
    }];
    let state = sized_state(MAX_PROVIDER_STATE_BYTES);
    assert!(build(&message, &[Some(&state)]).is_ok());
    let state = sized_state(MAX_PROVIDER_STATE_BYTES + 1);
    assert_eq!(
        build(&message, &[Some(&state)]).unwrap_err().code,
        "OpenAICodexProviderStateTooLarge"
    );
}

#[test]
fn openai_codex_replay_tool_count_accepts_the_limit_and_rejects_one_call_beyond() {
    let calls = |count| {
        (0..count)
            .map(|_| call("call", "read_file", "{}"))
            .collect()
    };
    assert!(build(&[assistant_calls(calls(MAX_TOOL_CALLS))], &[None]).is_ok());
    assert_eq!(
        build(&[assistant_calls(calls(MAX_TOOL_CALLS + 1))], &[None])
            .unwrap_err()
            .code,
        "OpenAICodexToolCallLimitExceeded"
    );
}

#[test]
fn openai_codex_replay_tool_identities_accept_the_limit_and_reject_one_byte_beyond() {
    let fits = "i".repeat(MAX_TOOL_IDENTITY_BYTES);
    let over = "i".repeat(MAX_TOOL_IDENTITY_BYTES + 1);
    assert!(build(&[assistant_calls(vec![call(&fits, "read", "{}")])], &[None]).is_ok());
    assert!(build(&[assistant_calls(vec![call("call", &fits, "{}")])], &[None]).is_ok());
    for calls in [
        vec![call(&over, "read", "{}")],
        vec![call("call", &over, "{}")],
    ] {
        assert_eq!(
            build(&[assistant_calls(calls)], &[None]).unwrap_err().code,
            "OpenAICodexToolCallLimitExceeded"
        );
    }
}

#[test]
fn openai_codex_replay_tool_arguments_accept_the_limit_and_reject_one_byte_beyond() {
    let fits = sized_arguments(MAX_TOOL_ARGUMENTS_BYTES);
    assert!(
        build(
            &[assistant_calls(vec![call("call", "read", &fits)])],
            &[None]
        )
        .is_ok()
    );
    let over = sized_arguments(MAX_TOOL_ARGUMENTS_BYTES + 1);
    assert_eq!(
        build(
            &[assistant_calls(vec![call("call", "read", &over)])],
            &[None]
        )
        .unwrap_err()
        .code,
        "OpenAICodexToolArgumentsTooLarge"
    );
}

#[test]
fn openai_codex_standard_requests_omit_the_priority_service_tier() {
    let body = build(&[ChatMessage::user("Hello.")], &[None]).unwrap();
    assert!(!body.contains("service_tier"));
    assert!(!body.contains("\"reasoning\""));
}

#[test]
fn openai_codex_rejects_invalid_models_and_system_messages() {
    for model in ["", "gpt 5", "gpt\u{7f}"] {
        let messages = [ChatMessage::user("Hello.")];
        let mut invalid = request(&messages, &[], &[]);
        invalid.model = model;
        assert_eq!(
            codex_failure(build_request(&invalid, &[None]).unwrap_err()).code,
            "InvalidOpenAICodexModel"
        );
    }
    assert_eq!(
        build(
            &[ChatMessage::System {
                content: "x".to_owned()
            }],
            &[None]
        )
        .unwrap_err()
        .code,
        "InvalidProviderPrompt"
    );
}

#[tokio::test]
async fn openai_codex_sse_maps_text_reasoning_tools_and_usage() {
    let sse = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\"}}\n\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"thinking\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"opaque\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"message\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"hello\"}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":2,\"delta\":\"{\\\"path\\\":\\\"README.md\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":4}}}\n\n",
    );
    let mut chunks = Chunks(VecDeque::from([sse.as_bytes().to_vec()]));
    let mut events = Vec::new();
    let mut sink = |event: StreamEvent| events.push(event);
    let completion = consume_stream(
        &mut chunks,
        &mut sink,
        &CancellationToken::new(),
        STREAM_LIMITS,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        events,
        [
            StreamEvent::ReasoningDelta {
                text: "thinking".to_owned()
            },
            StreamEvent::TextDelta {
                text: "hello".to_owned()
            },
        ]
    );
    assert_eq!(completion.tool_calls.len(), 1);
    assert_eq!(completion.tool_calls[0].id.as_str(), "call_1");
    assert_eq!(
        completion.tool_calls[0].arguments,
        r#"{"path":"README.md"}"#
    );
    assert_eq!(completion.usage.input_tokens, Some(10));
    assert!(
        completion
            .provider_state
            .as_deref()
            .is_some_and(|state| state.contains(r#""encrypted_content":"opaque""#))
    );
    assert_eq!(completion.finish, ResponsesFinish::ToolCalls);
}

#[tokio::test]
async fn openai_codex_rejects_cumulative_event_and_byte_limits() {
    let terminal = r#"{"type":"response.completed","response":{"status":"completed"}}"#;
    let terminal_event = format!("data: {terminal}\n\n");
    consume(
        &terminal_event,
        StreamLimits {
            events: 1,
            aggregate_bytes: terminal.len(),
            ..STREAM_LIMITS
        },
    )
    .await
    .unwrap();
    let event = "data: {\"type\":\"response.reasoning_summary_part.done\"}\n\n";
    expect_sse_error(
        "OpenAICodexResourceLimitExceeded",
        &format!("{event}{event}"),
        StreamLimits {
            events: 1,
            ..STREAM_LIMITS
        },
    )
    .await;
    expect_sse_error(
        "OpenAICodexResourceLimitExceeded",
        &terminal_event,
        StreamLimits {
            aggregate_bytes: terminal.len() - 1,
            ..STREAM_LIMITS
        },
    )
    .await;
}

#[tokio::test]
async fn openai_codex_rejects_oversized_streamed_tool_identities() {
    let limits = StreamLimits {
        tool_identity_bytes: 3,
        ..STREAM_LIMITS
    };
    expect_sse_error(
        "OpenAICodexToolCallLimitExceeded",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call\",\"name\":\"ok\"}}\n\n",
        limits,
    )
    .await;
    expect_sse_error(
        "OpenAICodexToolCallLimitExceeded",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"ok\",\"name\":\"read\"}}\n\n",
        limits,
    )
    .await;
}

#[tokio::test]
async fn openai_codex_bounds_every_streamed_argument_representation_and_cleans_staged_state() {
    let prefix = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"opaque\"}}\n\n",
    );
    for event in [
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"four\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"four\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"arguments\":\"four\"}}\n\n",
    ] {
        expect_sse_error(
            "OpenAICodexToolArgumentsTooLarge",
            &format!("{prefix}{event}"),
            StreamLimits {
                tool_arguments_bytes: 3,
                ..STREAM_LIMITS
            },
        )
        .await;
    }
}

#[tokio::test]
async fn openai_codex_rejects_oversized_encrypted_provider_state() {
    expect_sse_error(
        "OpenAICodexResourceLimitExceeded",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"opaque\"}}\n\n",
        StreamLimits {
            provider_state_bytes: 16,
            ..STREAM_LIMITS
        },
    )
    .await;
}

#[tokio::test]
async fn openai_codex_provider_state_accepts_the_exact_framed_limit() {
    let expected = r#"[{"type":"reasoning","encrypted_content":"a"},{"type":"reasoning","encrypted_content":"b"}]"#;
    let sse = concat!(
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"a\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"b\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    let completion = consume(
        sse,
        StreamLimits {
            provider_state_bytes: expected.len(),
            ..STREAM_LIMITS
        },
    )
    .await
    .unwrap();
    assert_eq!(completion.provider_state.as_deref(), Some(expected));
    expect_sse_error(
        "OpenAICodexResourceLimitExceeded",
        sse,
        StreamLimits {
            provider_state_bytes: expected.len() - 1,
            ..STREAM_LIMITS
        },
    )
    .await;
}

#[tokio::test]
async fn openai_codex_rejects_a_129th_streamed_tool_call() {
    let mut sse = String::new();
    for index in 0..=MAX_TOOL_CALLS {
        let _ = write!(
            sse,
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":{index},\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_{index}\",\"name\":\"read_file\"}}}}\n\n"
        );
    }
    sse.push_str(
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    expect_sse_error("OpenAICodexToolCallLimitExceeded", &sse, STREAM_LIMITS).await;
}

#[test]
fn replay_comes_only_from_assistant_messages_of_the_same_codex_model() {
    let replayed = |provider: &str, model: &str, parts: &str| ChatMessage::Assistant {
        content: Some("OK".to_owned()),
        tool_calls: Vec::new(),
        provider_replay: Some(ProviderReplay {
            source: ReplaySource {
                provider: provider.to_owned(),
                model: model.to_owned(),
            },
            parts_json: parts.to_owned(),
        }),
    };
    let messages = [
        ChatMessage::user("one"),
        replayed("codex", "gpt-5.6-sol", "[\"same\"]"),
        replayed("codex", "gpt-5.4", "[\"other model\"]"),
        replayed("grok", "gpt-5.6-sol", "[\"other provider\"]"),
        ChatMessage::Assistant {
            content: Some("OK".to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        },
    ];
    assert_eq!(
        replay_parts(&request(&messages, &[], &[])),
        [None, Some("[\"same\"]"), None, None, None]
    );
}

#[test]
fn codex_replay_projection_keeps_the_source_and_selects_parts() {
    let source = replay_source("gpt-5.6-sol");
    let replay = ProviderReplay {
        source: source.clone(),
        parts_json: r#"[{"type":"reasoning","encrypted_content":"cipher"},{"type":"message","phase":"commentary"}]"#.to_owned(),
    };
    let codex = CodexProvider::new(
        CodexAccess::new("token".to_owned(), "acct".to_owned(), i64::MAX),
        Arc::new(NoRefresh),
        "oh-fx/test",
        CodexEndpoints::default(),
    )
    .unwrap();
    assert_eq!(
        codex.project_replay(&replay, false, true),
        Ok(Some(ProviderReplay {
            source,
            parts_json: r#"[{"type":"reasoning","encrypted_content":"cipher"}]"#.to_owned(),
        }))
    );
    assert_eq!(codex.project_replay(&replay, false, false), Ok(None));
    let invalid = ProviderReplay {
        parts_json: "{}".to_owned(),
        ..replay
    };
    assert_eq!(
        codex
            .project_replay(&invalid, false, true)
            .unwrap_err()
            .code,
        "InvalidProviderState"
    );
}

#[tokio::test]
async fn errors_mask_the_token_their_request_sent_after_another_request_rotates_it() {
    const SENT: &str = "codex-sent-credential-0123456789";
    const ROTATED: &str = "codex-rotated-credential-9876543210";
    let echoed = format!("failed for {SENT}");
    let replies = [
        Reply::status(
            500,
            json!({"error": {"code": "server_error", "message": echoed}}).to_string(),
        ),
        Reply::sse(&[json!({"type": "response.failed", "response": {"error": {"code": "server_error", "message": echoed}}}).to_string()]),
    ];
    for reply in replies {
        let server = FakeServer::start([reply]);
        let codex = CodexProvider::new(
            CodexAccess::new(SENT.to_owned(), "acct".to_owned(), i64::MAX),
            Arc::new(Rotating(ROTATED)),
            "oh-fx/test",
            CodexEndpoints {
                responses: format!("{}/backend-api/codex/responses", server.base_url()),
            },
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let mut sent = Vec::new();
        let response = codex.post("{}", &mut sent, &cancel).await.unwrap();
        assert!(codex.replace_access(CodexRefresh::Force).await);
        assert_eq!(*codex.secrets(&sent), [SENT, ROTATED]);
        let mut sink = |_: StreamEvent| {};
        let error = codex
            .receive(response, &sent, &mut sink, &cancel, "gpt-5.6-sol")
            .await
            .unwrap_err();
        let rendered = format!("{error:?}");
        assert!(rendered.contains("failed for"), "{rendered}");
        assert!(!rendered.contains(SENT), "{rendered}");
        assert!(!rendered.contains(ROTATED), "{rendered}");
    }
}

#[tokio::test]
async fn a_retried_request_masks_every_token_it_sent() {
    const SENT: &str = "codex-sent-credential-0123456789";
    const ROTATED: &str = "codex-rotated-credential-9876543210";
    let server = FakeServer::start([
        Reply::status(
            401,
            json!({"error": {"code": "token_expired", "message": format!("expired {SENT}")}})
                .to_string(),
        ),
        Reply::status(
            500,
            json!({"error": {"code": "server_error", "message": format!("failed for {SENT} and {ROTATED}")}})
                .to_string(),
        ),
    ]);
    let codex = CodexProvider::new(
        CodexAccess::new(SENT.to_owned(), "acct".to_owned(), i64::MAX),
        Arc::new(Rotating(ROTATED)),
        "oh-fx/test",
        CodexEndpoints {
            responses: format!("{}/backend-api/codex/responses", server.base_url()),
        },
    )
    .unwrap();
    let messages = [ChatMessage::user("Hello.")];
    let request = request(&messages, &[], &[]);
    let mut sink = |_: StreamEvent| {};
    let error = codex
        .stream(&request, &mut sink, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.status, Some(500));
    let rendered = format!("{error:?}");
    assert!(rendered.contains("failed for"), "{rendered}");
    assert!(!rendered.contains(SENT), "{rendered}");
    assert!(!rendered.contains(ROTATED), "{rendered}");
}

#[tokio::test]
async fn rejected_stream_events_are_reported_without_their_bytes() {
    const TOKEN: &str = "codex-stream-credential-0123456789abcdef";
    let escaped = TOKEN.chars().fold(String::new(), |mut out, c| {
        let _ = write!(out, "\\u{:04x}", u32::from(c));
        out
    });
    let opened = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}"#;
    let rejected = format!(
        r#"{{"type":"response.output_text.delta","delta":5,"echo":"{escaped}","raw":"{TOKEN}"}}"#
    );
    let mut chunks = Chunks(VecDeque::from([format!(
        "data: {opened}\n\ndata: {rejected}\n\n"
    )
    .into_bytes()]));
    let mut sink = |_: StreamEvent| {};
    let error = consume_stream(
        &mut chunks,
        &mut sink,
        &CancellationToken::new(),
        STREAM_LIMITS,
        &[TOKEN.to_owned()],
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "InvalidOpenAICodexSseEvent");
    let detail = error.detail.expect("a detail");
    assert_eq!(
        detail,
        format!("stream event 2 ({} bytes) was rejected", rejected.len())
    );
    for fragment in [&TOKEN[..8], &escaped[..12], "\\u00"] {
        assert!(!detail.contains(fragment), "{detail}");
    }
}

#[tokio::test]
async fn streamed_failures_mask_a_token_before_bounding_the_diagnostic() {
    const TOKEN: &str = "codex-failure-credential-0123456789abcdef";
    let message = format!("{} {TOKEN} was rejected", "x".repeat(215));
    let start = "server_error: ".len() + message.find(TOKEN).unwrap_or_default();
    assert!(start + 16 < 253 && start + TOKEN.len() > 253);
    let failed = json!({"type": "response.failed", "response": {"error": {"code": "server_error", "message": message}}});
    let server = FakeServer::start([Reply::sse(&[failed.to_string()])]);
    let codex = CodexProvider::new(
        CodexAccess::new(TOKEN.to_owned(), "acct".to_owned(), i64::MAX),
        Arc::new(NoRefresh),
        "oh-fx/test",
        CodexEndpoints {
            responses: format!("{}/backend-api/codex/responses", server.base_url()),
        },
    )
    .unwrap();
    let messages = [ChatMessage::user("Hello.")];
    let request = request(&messages, &[], &[]);
    let mut sink = |_: StreamEvent| {};
    let error = codex
        .stream(&request, &mut sink, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::ServerError);
    let detail = error.detail.expect("a detail");
    let diagnostic = error.diagnostic.expect("a diagnostic");
    assert!(
        detail.starts_with("provider error: server_error: xxx"),
        "{detail}"
    );
    for shown in [&detail, &diagnostic] {
        assert!(!shown.contains(&TOKEN[..16]), "{shown}");
        assert!(shown.len() <= 256 + "provider error: ".len(), "{shown}");
    }
}

struct Rotating(&'static str);

impl CodexCredentials for Rotating {
    fn refresh<'a>(
        &'a self,
        _mode: CodexRefresh,
        account_id: &'a str,
    ) -> BoxFuture<'a, Option<CodexAccess>> {
        Box::pin(async move {
            Some(CodexAccess::new(
                self.0.to_owned(),
                account_id.to_owned(),
                i64::MAX,
            ))
        })
    }
}

struct NoRefresh;

impl CodexCredentials for NoRefresh {
    fn refresh<'a>(
        &'a self,
        _mode: CodexRefresh,
        _account_id: &'a str,
    ) -> BoxFuture<'a, Option<CodexAccess>> {
        Box::pin(async { None })
    }
}
