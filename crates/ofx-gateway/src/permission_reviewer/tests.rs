use std::collections::VecDeque;
use std::sync::Mutex;

use ofx_config::{MaxTokensParameter, ToolChoiceMode};
use ofx_contract::{
    ChatMessage, ProviderOptions, StreamSink, ToolCall, ToolCallId, ToolChoice, ToolResultStatus,
    ToolSpec,
};
use serde_json::Value;

use super::*;
use crate::chat_completions_protocol::{self, RequestOptions};
use crate::openai_codex;

struct Scripted {
    results: Mutex<VecDeque<Result<Completion, ProviderError>>>,
    sent: Mutex<Vec<(String, String)>>,
}

impl Scripted {
    fn new(results: impl IntoIterator<Item = Result<Completion, ProviderError>>) -> Arc<Self> {
        Arc::new(Self {
            results: Mutex::new(results.into_iter().collect()),
            sent: Mutex::new(Vec::new()),
        })
    }
}

impl ModelProvider for Scripted {
    fn stream<'a>(
        &'a self,
        _request: &'a ModelRequest<'a>,
        _sink: &'a mut dyn StreamSink,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        unreachable!("reviews send the body they built")
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        Some(format!("body for {}", request.model))
    }

    fn stream_body<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        sink: &'a mut dyn StreamSink,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        self.sent
            .lock()
            .unwrap()
            .push((request.model.to_owned(), body));
        sink.emit(StreamEvent::TextDelta {
            text: "{\"decision\":\"clear\"}".to_owned(),
        });
        let result = self.results.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { result })
    }
}

fn decision() -> Completion {
    Completion {
        content: Some("{\"decision\":\"clear\"}".to_owned()),
        tool_calls: vec![ToolCall {
            id: ToolCallId::new("review"),
            name: "permission_decision".to_owned(),
            arguments: "{\"decision\":\"caution\"}".to_owned(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: Usage {
            input_tokens: Some(9),
            output_tokens: Some(4),
        },
        provider_replay: None,
    }
}

const SCHEMA: &str = r#"{"type":"object","properties":{"decision":{"type":"string","enum":["clear","caution"]}},"required":["decision"],"additionalProperties":false}"#;

fn review_tools() -> [ToolSpec; 1] {
    [ToolSpec {
        name: "permission_decision".to_owned(),
        description: "Return bounded safety advice for one exact fx action.".to_owned(),
        input_schema: SCHEMA,
    }]
}

fn review_messages() -> [ChatMessage; 3] {
    let call = ToolCall {
        id: ToolCallId::new("call_review"),
        name: "shell".to_owned(),
        arguments: r#"{"action":"run","command":"rm -rf build"}"#.to_owned(),
    };
    [
        ChatMessage::user("review_context_kind: normal\n"),
        ChatMessage::Assistant {
            content: None,
            tool_calls: vec![call.clone()],
            provider_replay: None,
        },
        ChatMessage::Tool {
            call_id: call.id,
            tool_name: call.name,
            content: "Tool call has not executed; it is pending permission review.".to_owned(),
            status: ToolResultStatus::Failure,
        },
    ]
}

async fn send(transport: &dyn ReviewTransport, model: &str) -> ReviewTransportOutcome {
    let tools = review_tools();
    let messages = review_messages();
    let request = ModelRequest {
        model,
        instructions: &["<permission_review>"],
        messages: &messages,
        tools: &tools,
        tool_choice: ToolChoice::Required,
        max_output_tokens: Some(transport.max_output_tokens(model)),
        provider_options: ProviderOptions::default(),
        session_id: None,
    };
    let body = transport.request_body(&request).unwrap();
    transport
        .send(&request, body, &CancellationToken::new())
        .await
}

fn failure(kind: ProviderErrorKind, code: &str) -> Result<Completion, ProviderError> {
    Err(ProviderError::new(kind, code))
}

#[test]
fn codex_reviews_use_the_catalog_reviewer_model_and_the_tested_output_budget() {
    let transport = CodexReviewTransport::new(Scripted::new([]));
    assert_eq!(transport.model("gpt-5.5"), "gpt-5.6-luna");
    assert_eq!(transport.max_output_tokens("gpt-5.6-luna"), 2048);
}

#[test]
fn custom_connection_reviews_use_the_configured_reviewer_or_the_source_model() {
    let configured = ChatCompletionsReviewTransport::new(
        Scripted::new([]),
        Some("openai/review".to_owned()),
        |_| None,
    );
    assert_eq!(configured.model("vendor/main"), "openai/review");
    let unconfigured =
        ChatCompletionsReviewTransport::new(Scripted::new([]), None, |model| match model {
            "small" => Some(1024),
            "large" => Some(65_536),
            _ => None,
        });
    assert_eq!(unconfigured.model("vendor/main"), "vendor/main");
    assert_eq!(unconfigured.max_output_tokens("small"), 1024);
    assert_eq!(unconfigured.max_output_tokens("large"), 2048);
    assert_eq!(unconfigured.max_output_tokens("unknown"), 2048);
}

#[tokio::test]
async fn reviews_return_the_structured_completion_and_never_the_streamed_text() {
    let provider = Scripted::new([Ok(decision())]);
    let transport = CodexReviewTransport::new(provider.clone());
    assert_eq!(
        send(&transport, "gpt-5.6-luna").await,
        ReviewTransportOutcome::Completion(decision())
    );
    assert_eq!(
        provider.sent.lock().unwrap()[..],
        [(
            "gpt-5.6-luna".to_owned(),
            "body for gpt-5.6-luna".to_owned()
        )]
    );
}

#[tokio::test]
async fn codex_review_failures_follow_the_responses_reviewer() {
    use ProviderErrorKind as Kind;
    use ReviewTransportOutcome as Outcome;
    let undecided = Outcome::Completion(undecided());
    for (result, expected) in [
        (failure(Kind::Cancelled, "Cancelled"), Outcome::Cancelled),
        (failure(Kind::Timeout, "Timeout"), Outcome::TimedOut),
        (
            failure(Kind::RateLimited, "RateLimited"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::ServerError, "ServerError"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::BadGateway, "BadGateway"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::Unavailable, "Unavailable"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::GatewayTimeout, "GatewayTimeout"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::ProviderError, "ProviderError"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::ConnectionFailed, "ConnectionRefused"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::ConnectivityLost, "ConnectivityLost"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::TransportInterrupted, "ReadFailed"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::Protocol, "OpenAICodexStreamIncomplete"),
            Outcome::TransientFailure,
        ),
        (
            failure(Kind::Protocol, "OutputTruncated"),
            undecided.clone(),
        ),
        (
            failure(Kind::ProviderError, "ContentFiltered"),
            Outcome::PermanentFailure,
        ),
        (
            failure(Kind::InvalidRequest, "InvalidRequest"),
            Outcome::PermanentFailure,
        ),
        (
            failure(Kind::Unauthorized, "Unauthorized"),
            Outcome::PermanentFailure,
        ),
        (
            failure(Kind::Forbidden, "Forbidden"),
            Outcome::PermanentFailure,
        ),
        (
            failure(Kind::RequestTooLarge, "RequestTooLarge"),
            Outcome::PermanentFailure,
        ),
    ] {
        let transport = CodexReviewTransport::new(Scripted::new([result.clone()]));
        assert_eq!(
            send(&transport, "gpt-5.6-luna").await,
            expected,
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn custom_connection_review_failures_are_permanent_except_timeouts_and_missing_decisions() {
    use ProviderErrorKind as Kind;
    use ReviewTransportOutcome as Outcome;
    for (result, expected) in [
        (failure(Kind::Cancelled, "Cancelled"), Outcome::Cancelled),
        (failure(Kind::Timeout, "Timeout"), Outcome::TimedOut),
        (
            failure(Kind::Protocol, "RequiredToolMissing"),
            Outcome::Completion(undecided()),
        ),
        (
            failure(Kind::RateLimited, "RateLimited"),
            Outcome::PermanentFailure,
        ),
        (
            failure(Kind::ServerError, "ServerError"),
            Outcome::PermanentFailure,
        ),
        (
            failure(Kind::ConnectionFailed, "ConnectionRefused"),
            Outcome::PermanentFailure,
        ),
        (
            failure(Kind::Protocol, "OutputTruncated"),
            Outcome::PermanentFailure,
        ),
        (
            failure(Kind::Protocol, "ContentFiltered"),
            Outcome::PermanentFailure,
        ),
        (
            failure(Kind::Unauthorized, "Unauthorized"),
            Outcome::PermanentFailure,
        ),
    ] {
        let transport =
            ChatCompletionsReviewTransport::new(Scripted::new([result.clone()]), None, |_| None);
        assert_eq!(
            send(&transport, "vendor/main").await,
            expected,
            "{result:?}"
        );
    }
}

#[test]
fn codex_review_bodies_name_the_reviewer_require_the_decision_and_close_the_pending_call() {
    let tools = review_tools();
    let messages = review_messages();
    let request = ModelRequest {
        model: CODEX_REVIEWER_MODEL,
        instructions: &["<permission_review>"],
        messages: &messages,
        tools: &tools,
        tool_choice: ToolChoice::Required,
        max_output_tokens: Some(MAX_REVIEW_OUTPUT_TOKENS),
        provider_options: ProviderOptions::default(),
        session_id: None,
    };
    let body: Value =
        serde_json::from_str(&openai_codex::build_request(&request, &[None, None, None]).unwrap())
            .unwrap();
    assert_eq!(body["model"], "gpt-5.6-luna");
    assert_eq!(body["instructions"], "<permission_review>");
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["tools"][0]["name"], "permission_decision");
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["input"][2]["call_id"], "call_review");
    assert!(body.get("reasoning").is_none());
    assert!(body.get("service_tier").is_none());
    assert!(body.get("max_output_tokens").is_none());
}

#[test]
fn custom_connection_review_bodies_carry_the_review_budget() {
    let tools = review_tools();
    let messages = review_messages();
    let request = ModelRequest {
        model: "openai/review",
        instructions: &["<permission_review>"],
        messages: &messages,
        tools: &tools,
        tool_choice: ToolChoice::Required,
        max_output_tokens: Some(1024),
        provider_options: ProviderOptions::default(),
        session_id: None,
    };
    let prepared = chat_completions_protocol::build_request(
        &request,
        RequestOptions {
            tool_choice_mode: ToolChoiceMode::Send,
            max_tokens_parameter: MaxTokensParameter::MaxTokens,
        },
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    assert_eq!(body["model"], "openai/review");
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(body["messages"][3]["tool_call_id"], "call_review");
}
