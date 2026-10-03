use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ofx_contract::{
    BoxFuture, ChatMessage, Completion, FinishReason, ModelProvider, ModelRequest, ProviderError,
    ProviderErrorKind, ProviderOptions, ProviderReplay, ReplaySource, StreamEvent, ToolCall,
    ToolCallId, ToolChoice, ToolResultStatus, ToolSpec,
};
use ofx_gateway::{CodexAccess, CodexCredentials, CodexEndpoints, CodexProvider, CodexRefresh};
use ofx_testkit::{FakeServer, RecordedRequest, Reply};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWNyZXQtYWNjZXNzIn0.c2lnbmF0dXJl";
const FRESH_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJmcmVzaC1hY2Nlc3MifQ.ZnJlc2g";
const ACCOUNT: &str = "acct_test";
const FAR_FUTURE_MS: i64 = 4_102_444_800_000;
const TOOL_STEP_GOLDEN: &str = include_str!("golden/codex_tool_step.json");
const AFTER_TOOL_GOLDEN: &str = include_str!("golden/codex_after_tool.json");

#[derive(Default)]
struct FakeCredentials {
    replies: Mutex<VecDeque<Option<(String, i64)>>>,
    calls: Mutex<Vec<(CodexRefresh, String)>>,
}

impl FakeCredentials {
    fn replying(replies: impl IntoIterator<Item = Option<(&'static str, i64)>>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|reply| reply.map(|(token, after)| (token.to_owned(), after)))
                    .collect(),
            ),
            calls: Mutex::default(),
        })
    }

    fn calls(&self) -> Vec<(CodexRefresh, String)> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl CodexCredentials for FakeCredentials {
    fn refresh<'a>(
        &'a self,
        mode: CodexRefresh,
        account_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<CodexAccess>> {
        self.calls
            .lock()
            .expect("calls lock")
            .push((mode, account_id.to_owned()));
        let reply = self
            .replies
            .lock()
            .expect("replies lock")
            .pop_front()
            .flatten();
        Box::pin(async move {
            reply.map(|(token, after)| CodexAccess::new(token, ACCOUNT.to_owned(), after))
        })
    }
}

fn provider(
    server: &FakeServer,
    credentials: Arc<FakeCredentials>,
    refresh_after_ms: i64,
) -> CodexProvider {
    CodexProvider::new(
        CodexAccess::new(TOKEN.to_owned(), ACCOUNT.to_owned(), refresh_after_ms),
        credentials,
        "oh-fx/test",
        CodexEndpoints {
            responses: format!("{}/backend-api/codex/responses", server.base_url()),
        },
    )
    .expect("build the Codex provider")
}

fn user(text: &str) -> Vec<ChatMessage> {
    vec![ChatMessage::user(text)]
}

fn request<'a>(
    instructions: &'a [&'a str],
    messages: &'a [ChatMessage],
    tools: &'a [ToolSpec],
) -> ModelRequest<'a> {
    ModelRequest {
        model: "gpt-5.4",
        instructions,
        messages,
        tools,
        tool_choice: ToolChoice::Auto,
        max_output_tokens: Some(4096),
        provider_options: ProviderOptions::default(),
        session_id: None,
    }
}

async fn run(
    provider: &CodexProvider,
    instructions: &[&str],
    messages: &[ChatMessage],
    tools: &[ToolSpec],
) -> (Result<Completion, ProviderError>, Vec<StreamEvent>) {
    let request = request(instructions, messages, tools);
    let mut events = Vec::new();
    let mut sink = |event: StreamEvent| events.push(event);
    let result = provider
        .stream(&request, &mut sink, &CancellationToken::new())
        .await;
    (result, events)
}

fn tool_step_events() -> Vec<String> {
    [
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"Thinking"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque-cipher"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1","phase":"commentary"}}),
        json!({"type":"response.output_text.delta","output_index":1,"delta":"I will read it."}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":\"README.md\"}"}),
        json!({"type":"response.function_call_arguments.done","output_index":2,"arguments":"{\"path\":\"README.md\"}"}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect()
}

fn text_events(text: &str) -> Vec<String> {
    [
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_2","phase":"final_answer"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":text}),
        json!({"type":"response.completed","response":{"id":"resp_2","status":"completed","usage":{"input_tokens":20,"output_tokens":3}}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect()
}

fn reasoned_text_events(cipher: &str, text: &str) -> Vec<String> {
    [
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":format!("rs_{cipher}")}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":format!("rs_{cipher}"),"type":"reasoning","summary":[],"encrypted_content":cipher}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":format!("msg_{cipher}"),"phase":"final_answer"}}),
        json!({"type":"response.output_text.delta","output_index":1,"delta":text}),
        json!({"type":"response.completed","response":{"id":format!("resp_{cipher}"),"status":"completed","usage":{"input_tokens":5,"output_tokens":1}}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect()
}

fn answered(completion: &Completion) -> ChatMessage {
    ChatMessage::Assistant {
        content: completion.content.clone(),
        tool_calls: completion.tool_calls.clone(),
        provider_replay: completion.provider_replay.clone(),
    }
}

fn ciphers(request: &RecordedRequest) -> Vec<String> {
    request.json()["input"]
        .as_array()
        .expect("input items")
        .iter()
        .filter_map(|item| item["encrypted_content"].as_str().map(str::to_owned))
        .collect()
}

fn golden_tools(golden: &Value) -> Vec<ToolSpec> {
    golden["tools"]
        .as_array()
        .expect("golden tools")
        .iter()
        .map(|tool| ToolSpec {
            name: tool["name"].as_str().expect("tool name").to_owned(),
            description: tool["description"].as_str().unwrap_or_default().to_owned(),
            input_schema: tool["parameters"].to_string().leak(),
        })
        .collect()
}

fn assert_codex_headers(request: &RecordedRequest, token: &str) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/backend-api/codex/responses");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(request.header("chatgpt-account-id"), Some(ACCOUNT));
    assert_eq!(request.header("originator"), Some("oh-fx"));
    assert_eq!(
        request.header("openai-beta"),
        Some("responses=experimental")
    );
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("user-agent"), Some("oh-fx/test"));
    assert_eq!(request.header("session-id"), None);
    assert_eq!(request.header("x-client-request-id"), None);
}

#[tokio::test]
async fn saved_sessions_name_themselves_on_every_request_and_its_replay() {
    let server = FakeServer::start([
        Reply::status(
            401,
            r#"{"error":{"code":"token_expired","message":"expired"}}"#,
        ),
        Reply::sse(&text_events("after refresh")),
        Reply::sse(&text_events("unsaved")),
    ]);
    let credentials = FakeCredentials::replying([Some((FRESH_TOKEN, FAR_FUTURE_MS))]);
    let codex = provider(&server, credentials, FAR_FUTURE_MS);
    let messages = user("Hello.");
    for session_id in [Some("abcdefghijkl"), Some("")] {
        let request = ModelRequest {
            session_id,
            ..request(&[], &messages, &[])
        };
        codex
            .stream(&request, &mut |_| {}, &CancellationToken::new())
            .await
            .expect("completes");
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[..2] {
        assert_eq!(request.header("session-id"), Some("abcdefghijkl"));
        assert_eq!(request.header("x-client-request-id"), Some("abcdefghijkl"));
    }
    assert_codex_headers(&requests[2], FRESH_TOKEN);
}

#[tokio::test]
async fn request_bodies_match_upstream_byte_for_byte_across_a_tool_step() {
    let server = FakeServer::start([
        Reply::sse(&tool_step_events()),
        Reply::sse(&text_events("Done reading.")),
    ]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let golden: Value = serde_json::from_str(AFTER_TOOL_GOLDEN).expect("golden parses");
    let instructions = [golden["instructions"].as_str().expect("instructions")];
    let tools = golden_tools(&golden);
    let mut history = user("Read README.md");

    let (first, events) = run(&codex, &instructions, &history, &tools).await;
    let first = first.expect("tool step completes");
    assert_eq!(first.finish_reason, FinishReason::ToolCalls);
    assert_eq!(first.content.as_deref(), Some("I will read it."));
    assert_eq!(
        first.tool_calls,
        [ToolCall {
            id: ToolCallId::new("call_1"),
            name: "read_file".to_owned(),
            arguments: r#"{"path":"README.md"}"#.to_owned(),
        }]
    );
    assert_eq!(first.usage.input_tokens, Some(10));
    assert_eq!(
        events,
        [
            StreamEvent::Admitted,
            StreamEvent::ReasoningDelta {
                text: "Thinking".to_owned()
            },
            StreamEvent::TextDelta {
                text: "I will read it.".to_owned()
            },
        ]
    );

    let output = golden["input"][4]["output"].as_str().expect("tool output");
    assert_eq!(
        first.provider_replay.as_ref().map(|replay| &replay.source),
        Some(&ReplaySource {
            provider: "codex".to_owned(),
            model: "gpt-5.4".to_owned(),
        })
    );
    history.push(answered(&first));
    history.push(ChatMessage::Tool {
        call_id: ToolCallId::new("call_1"),
        tool_name: "read_file".to_owned(),
        content: output.to_owned(),
        status: ToolResultStatus::Success,
    });
    assert_eq!(
        codex
            .request_body(&request(&instructions, &history, &tools))
            .as_deref(),
        Some(AFTER_TOOL_GOLDEN)
    );
    let (second, _) = run(&codex, &instructions, &history, &tools).await;
    let second = second.expect("final step completes");
    assert_eq!(second.content.as_deref(), Some("Done reading."));
    assert_eq!(second.finish_reason, FinishReason::Stop);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_codex_headers(&requests[0], TOKEN);
    assert_eq!(requests[0].body_text(), TOOL_STEP_GOLDEN);
    assert_eq!(requests[1].body_text(), AFTER_TOOL_GOLDEN);
}

#[tokio::test]
async fn requests_without_instructions_or_tools_use_upstream_defaults() {
    let server = FakeServer::start([Reply::sse(&text_events("hi"))]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let (result, _) = run(&codex, &["", ""], &user("Hello."), &[]).await;
    result.expect("completes");
    assert_eq!(
        server.requests()[0].body_text(),
        r#"{"model":"gpt-5.4","store":false,"stream":true,"instructions":"You are a helpful assistant.","input":[{"role":"user","content":[{"type":"input_text","text":"Hello."}]}],"tool_choice":"auto","parallel_tool_calls":true,"include":["reasoning.encrypted_content"],"text":{"verbosity":"low"}}"#
    );
}

#[tokio::test]
async fn an_unauthorized_response_refreshes_once_and_replays_the_request() {
    let server = FakeServer::start([
        Reply::status(
            401,
            r#"{"error":{"code":"token_expired","message":"expired"}}"#,
        ),
        Reply::sse(&text_events("after refresh")),
    ]);
    let credentials = FakeCredentials::replying([Some((FRESH_TOKEN, FAR_FUTURE_MS))]);
    let codex = provider(&server, Arc::clone(&credentials), FAR_FUTURE_MS);
    let (result, _) = run(&codex, &[], &user("Hello."), &[]).await;
    assert_eq!(
        result.expect("replay succeeds").content.as_deref(),
        Some("after refresh")
    );
    assert_eq!(
        credentials.calls(),
        [(CodexRefresh::Force, ACCOUNT.to_owned())]
    );
    let requests = server.requests();
    assert_codex_headers(&requests[0], TOKEN);
    assert_codex_headers(&requests[1], FRESH_TOKEN);
    assert_eq!(requests[0].body, requests[1].body);
}

#[tokio::test]
async fn a_rejected_refresh_reports_the_unauthorized_response_without_the_token() {
    let body = format!(r#"{{"error":{{"code":"invalid_token","message":"bad token {TOKEN}"}}}}"#);
    let server = FakeServer::start([Reply::status(401, body)]);
    let credentials = FakeCredentials::replying([None]);
    let codex = provider(&server, credentials, FAR_FUTURE_MS);
    let (result, _) = run(&codex, &[], &user("Hello."), &[]).await;
    let error = result.expect_err("unauthorized");
    assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
    assert_eq!(error.status, Some(401));
    let rendered = format!("{error:?}");
    assert!(!rendered.contains(TOKEN), "{rendered}");
    assert!(
        error.detail.as_deref().is_some_and(
            |detail| detail.starts_with("API access denied · HTTP 401 · invalid_token")
        )
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn rate_limits_carry_retry_after_for_the_agent_recovery() {
    let server = FakeServer::start([Reply::status_with_headers(
        429,
        &[("Retry-After", "7")],
        r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached"}}"#,
    )]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let (result, _) = run(&codex, &[], &user("Hello."), &[]).await;
    let error = result.expect_err("rate limited");
    assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
    assert_eq!(
        error.detail.as_deref(),
        Some(
            "API request failed · HTTP 429 · usage_limit_reached: The usage limit has been reached"
        )
    );
}

#[tokio::test]
async fn an_expired_access_token_is_refreshed_before_the_request() {
    let server = FakeServer::start([
        Reply::sse(&text_events("fresh")),
        Reply::sse(&text_events("still fresh")),
    ]);
    let credentials = FakeCredentials::replying([Some((FRESH_TOKEN, FAR_FUTURE_MS))]);
    let codex = provider(&server, Arc::clone(&credentials), 0);
    run(&codex, &[], &user("Hello."), &[])
        .await
        .0
        .expect("completes");
    assert_eq!(
        credentials.calls(),
        [(CodexRefresh::IfNeeded, ACCOUNT.to_owned())]
    );
    assert_codex_headers(&server.requests()[0], FRESH_TOKEN);
    run(&codex, &[], &user("Again."), &[])
        .await
        .0
        .expect("completes");
    assert_eq!(credentials.calls().len(), 1);
}

#[tokio::test]
async fn stream_failures_map_to_retryable_and_terminal_provider_errors() {
    let failed = |code: &str| {
        vec![json!({"type":"response.failed","response":{"error":{"code":code,"message":"try later"}}}).to_string()]
    };
    let server = FakeServer::start([
        Reply::sse(&failed("server_error")),
        Reply::sse(&failed("rate_limit_exceeded")),
        Reply::sse(&failed("invalid_prompt")),
        Reply::sse(&["{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}"]),
    ]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    for expected in [
        ProviderErrorKind::ServerError,
        ProviderErrorKind::RateLimited,
        ProviderErrorKind::ProviderError,
    ] {
        let error = run(&codex, &[], &user("Hello."), &[])
            .await
            .0
            .expect_err("failure");
        assert_eq!(error.kind, expected);
        assert_eq!(error.code, "ProviderError");
        assert!(
            error
                .detail
                .as_deref()
                .is_some_and(|detail| detail.ends_with(": try later"))
        );
    }
    let incomplete = run(&codex, &[], &user("Hello."), &[])
        .await
        .0
        .expect_err("incomplete");
    assert_eq!(incomplete.code, "OpenAICodexStreamIncomplete");
}

#[tokio::test]
async fn malformed_call_arguments_arrive_as_sent_and_replay_only_as_an_empty_object() {
    let malformed = r#"{"path":"a",}"#;
    let call_events: Vec<String> = [
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":""}}),
        json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":malformed}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect();
    let server = FakeServer::start([
        Reply::sse(&call_events),
        Reply::sse(&text_events("Recovered.")),
    ]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let golden: Value = serde_json::from_str(AFTER_TOOL_GOLDEN).expect("golden parses");
    let tools = golden_tools(&golden);
    let prompt = user("Read a");

    let (first, _) = run(&codex, &[], &prompt, &tools).await;
    let first = first.expect("a malformed call still completes the step");
    assert_eq!(first.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        first.tool_calls,
        [ToolCall {
            id: ToolCallId::new("call_1"),
            name: "read_file".to_owned(),
            arguments: malformed.to_owned(),
        }]
    );

    let result = ChatMessage::Tool {
        call_id: ToolCallId::new("call_1"),
        tool_name: "read_file".to_owned(),
        content: "rejected".to_owned(),
        status: ToolResultStatus::Failure,
    };
    let as_sent = [prompt[0].clone(), answered(&first), result.clone()];
    let (refused, _) = run(&codex, &[], &as_sent, &tools).await;
    assert_eq!(
        refused.expect_err("raw arguments are never replayed").code,
        "InvalidToolArguments"
    );
    assert_eq!(server.requests().len(), 1);

    let replayed = ChatMessage::Assistant {
        content: None,
        tool_calls: vec![ToolCall {
            arguments: "{}".to_owned(),
            ..first.tool_calls[0].clone()
        }],
        provider_replay: first.provider_replay.clone(),
    };
    let history = [prompt[0].clone(), replayed, result];
    let (second, _) = run(&codex, &[], &history, &tools).await;
    let second = second.expect("the empty object replays");
    assert_eq!(second.content.as_deref(), Some("Recovered."));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let input = requests[1].json()["input"].clone();
    assert_eq!(
        input[1],
        json!({"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{}"})
    );
    assert_eq!(
        input[2],
        json!({"type":"function_call_output","call_id":"call_1","output":"rejected"})
    );
}

#[tokio::test]
async fn invalid_models_fail_before_any_request() {
    let server = FakeServer::start([]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let request = ModelRequest {
        model: "gpt 5",
        instructions: &[],
        messages: &user("Hello."),
        tools: &[],
        tool_choice: ToolChoice::Auto,
        max_output_tokens: None,
        provider_options: ProviderOptions::default(),
        session_id: None,
    };
    let error = codex
        .stream(&request, &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("invalid model");
    assert_eq!(error.code, "InvalidOpenAICodexModel");
    assert!(server.requests().is_empty());
}

#[test]
fn access_tokens_never_appear_in_debug_output() {
    let server = FakeServer::start([]);
    let access = CodexAccess::new(TOKEN.to_owned(), ACCOUNT.to_owned(), 0);
    assert!(!format!("{access:?}").contains(TOKEN));
    let codex = provider(&server, FakeCredentials::replying([]), 0);
    assert!(!format!("{codex:?}").contains(TOKEN));
}

#[tokio::test]
async fn interleaved_conversations_keep_their_own_encrypted_reasoning() {
    let server = FakeServer::start([
        Reply::sse(&reasoned_text_events("cipher-a", "A answers.")),
        Reply::sse(&reasoned_text_events("cipher-b", "B answers.")),
        Reply::sse(&text_events("A again.")),
        Reply::sse(&text_events("B again.")),
    ]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let mut first = user("A?");
    let mut second = user("B?");
    for conversation in [&mut first, &mut second] {
        let (result, _) = run(&codex, &[], conversation, &[]).await;
        conversation.push(answered(&result.expect("answers")));
        conversation.push(ChatMessage::user("And then?"));
    }
    for conversation in [&first, &second] {
        run(&codex, &[], conversation, &[])
            .await
            .0
            .expect("follows up");
    }
    let requests = server.requests();
    assert_eq!(ciphers(&requests[2]), ["cipher-a"]);
    assert_eq!(ciphers(&requests[3]), ["cipher-b"]);
}

#[tokio::test]
async fn identical_replies_replay_their_own_reasoning_in_order() {
    let server = FakeServer::start([
        Reply::sse(&reasoned_text_events("cipher-1", "OK")),
        Reply::sse(&reasoned_text_events("cipher-2", "OK")),
        Reply::sse(&text_events("Done.")),
    ]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let mut history = user("First?");
    for follow_up in ["Second?", "Third?"] {
        let (result, _) = run(&codex, &[], &history, &[]).await;
        history.push(answered(&result.expect("answers")));
        history.push(ChatMessage::user(follow_up));
    }
    run(&codex, &[], &history, &[]).await.0.expect("answers");
    assert_eq!(ciphers(&server.requests()[2]), ["cipher-1", "cipher-2"]);
}

#[tokio::test]
async fn replay_from_another_model_or_provider_is_omitted() {
    let server = FakeServer::start([Reply::sse(&text_events("Done."))]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let replayed = |provider: &str, model: &str, cipher: &str| ChatMessage::Assistant {
        content: Some("OK".to_owned()),
        tool_calls: Vec::new(),
        provider_replay: Some(ProviderReplay {
            source: ReplaySource {
                provider: provider.to_owned(),
                model: model.to_owned(),
            },
            parts_json: json!([{"type": "reasoning", "encrypted_content": cipher}]).to_string(),
        }),
    };
    let history = [
        ChatMessage::user("One?"),
        replayed("codex", "gpt-5.3", "other-model"),
        ChatMessage::user("Two?"),
        replayed("grok", "gpt-5.4", "other-provider"),
        ChatMessage::user("Three?"),
        replayed("codex", "gpt-5.4", "same-route"),
        ChatMessage::user("Four?"),
    ];
    run(&codex, &[], &history, &[]).await.0.expect("answers");
    let request = &server.requests()[0];
    assert_eq!(ciphers(request), ["same-route"]);
    let input = request.json()["input"].clone();
    let texts: Vec<&str> = input
        .as_array()
        .expect("input items")
        .iter()
        .filter(|item| item["role"] == "assistant")
        .map(|item| item["content"][0]["text"].as_str().expect("assistant text"))
        .collect();
    assert_eq!(texts, ["OK", "OK", "OK"]);
}

#[tokio::test]
async fn a_measured_body_is_sent_as_it_was_measured() {
    let server = FakeServer::start([Reply::sse(&text_events("hi"))]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let history = user("Hello.");
    let body =
        r#"{"model":"gpt-5.4","store":false,"stream":true,"instructions":"measured","input":[]}"#;
    let mut sink = |_: StreamEvent| {};
    let outcome = codex
        .stream_body(
            &request(&[], &history, &[]),
            body.to_owned(),
            &mut sink,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(outcome.expect("completes").content.as_deref(), Some("hi"));
    assert_eq!(server.requests()[0].body_text(), body);
}

#[tokio::test]
async fn a_context_overflow_failure_keeps_its_code_in_the_detail() {
    let failed = json!({"type":"response.failed","response":{"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}}});
    let server = FakeServer::start([Reply::sse(&[failed.to_string()])]);
    let codex = provider(&server, FakeCredentials::replying([]), FAR_FUTURE_MS);
    let error = run(&codex, &[], &user("Hello."), &[])
        .await
        .0
        .expect_err("failure");
    assert_eq!(error.kind, ProviderErrorKind::ProviderError);
    assert_eq!(
        error.detail.as_deref(),
        Some(
            "provider error: context_length_exceeded: Your input exceeds the context window of this model."
        )
    );
}
