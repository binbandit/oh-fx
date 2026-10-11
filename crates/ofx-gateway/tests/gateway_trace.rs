use std::env;
use std::fs;
use std::process::Command;

use ofx_contract::{
    ChatMessage, ModelProvider, ModelRequest, ProviderOptions, StreamEvent, ToolChoice,
};
use ofx_gateway::{GatewayCredential, GatewayEndpoints, GatewayProvider};
use ofx_testkit::{FakeServer, Reply};
use tokio_util::sync::CancellationToken;

const KEY: &str = "vck_live_0123456789abcdef";
const CHILD: &str = "OH_FX_GATEWAY_TRACE_CHILD";
const TEST: &str = "the_trace_log_names_every_gateway_step_and_never_the_key";
const MODEL: &str = "openai/gpt-5.6-sol";
const TOOL_EVENTS: [&str; 10] = [
    r#"{"type":"tool-input-start","id":"X","toolName":"read_file"}"#,
    r#"{"type":"tool-input-start","id":"X","toolName":"grep_files"}"#,
    r#"{"type":"tool-input-delta","id":"X","delta":"{\"path\":\"stable.txt\"}"}"#,
    r#"{"type":"tool-input-end","id":"X"}"#,
    r#"{"type":"tool-input-delta","id":"X","delta":"LATE_PREFIX_SENTINEL"}"#,
    r#"{"type":"tool-input-end","id":"X"}"#,
    r#"{"type":"tool-input-start","id":"X","toolName":"read_file"}"#,
    r#"{"type":"tool-call","toolCallId":"c2","toolName":"ask_user_question","input":"{]FX_ARGUMENT_PRIVACY_SENTINEL"}"#,
    r#"{"type":"tool-call","toolCallId":"X"}"#,
    r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#,
];

fn echoing_stream() -> Reply {
    let events = [
        format!(r#"{{"type":"response-metadata","modelId":"openai/{KEY}"}}"#),
        r#"{"type":"text-delta","id":"t","delta":"partial"}"#.to_owned(),
        format!(r#"{{"type":"error","error":{{"code":"leak","message":"key {KEY} rejected"}}}}"#),
        r#"{"type":"finish","finishReason":{"unified":"error"}}"#.to_owned(),
    ];
    let body = events.iter().fold(String::new(), |mut body, event| {
        body.push_str("data: ");
        body.push_str(event);
        body.push_str("\n\n");
        body
    });
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nx-vercel-ai-gateway-model: openai/{KEY}\r\nConnection: close\r\n\r\n"
    );
    Reply::Raw(format!("{head}{body}").into_bytes())
}

async fn run_turns() {
    let server = FakeServer::start([
        echoing_stream(),
        Reply::status(
            401,
            format!(
                r#"{{"error":{{"code":"invalid_api_key","message":"key {KEY} is not valid"}}}}"#
            ),
        ),
        Reply::sse(&[r#"{"type":"text-delta","delta":"cut"}"#, "[DONE]"]),
        Reply::sse(&["{not-json}"]),
        Reply::sse(&TOOL_EVENTS),
    ]);
    let gateway = GatewayProvider::new(
        GatewayCredential::new(Some(KEY.to_owned()), Some("team_123".to_owned())),
        "oh-fx/test",
        GatewayEndpoints {
            chat: format!("{}/v4/ai/language-model", server.base_url()),
        },
    )
    .expect("the gateway client builds");
    let messages = [ChatMessage::user(format!("my key is {KEY}"))];
    let request = ModelRequest {
        model: MODEL,
        instructions: &[],
        messages: &messages,
        tools: &[],
        tool_choice: ToolChoice::Auto,
        max_output_tokens: None,
        provider_options: ProviderOptions::default(),
        session_id: Some("session_123"),
    };
    for completes in [false, false, false, false, true] {
        let mut sink = |_: StreamEvent| {};
        let outcome = gateway
            .stream(&request, &mut sink, &CancellationToken::new())
            .await;
        assert_eq!(outcome.is_ok(), completes);
    }
}

fn run_child() {
    let workspace = tempfile::tempdir().expect("a temporary workspace");
    ofx_trace::configure_from_env(workspace.path(), None);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(run_turns());
}

fn position(log: &str, needle: &str) -> usize {
    log.find(needle)
        .unwrap_or_else(|| panic!("the log has no `{needle}`:\n{log}"))
}

#[test]
fn the_trace_log_names_every_gateway_step_and_never_the_key() {
    if env::var_os(CHILD).is_some() {
        run_child();
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let log_path = directory.path().join("trace.log");
    let status = Command::new(env::current_exe().unwrap())
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("OH_FX_TRACE_LOG", &log_path)
        .env_remove("OH_FX_TRACE")
        .env_remove("OH_FX_TRACE_SCOPES")
        .env_remove("OH_FX_TRACE_STDERR")
        .status()
        .unwrap();
    assert!(status.success());
    let log = fs::read_to_string(&log_path).unwrap();
    for fragment in [
        KEY,
        "vck_live",
        "0123456789abcdef",
        "LATE_PREFIX_SENTINEL",
        "FX_ARGUMENT_PRIVACY_SENTINEL",
    ] {
        assert!(
            !log.contains(fragment),
            "the log shows `{fragment}`:\n{log}"
        );
    }
    let ordered = [
        "[gateway] event=before_http_open_connect attempt=1 attempt_limit=1 retries_used=0",
        "[gateway] event=before_request_open attempt=1 attempt_limit=1 retries_used=0 payload_bytes=",
        "[gateway] event=after_http_open_connect attempt=1",
        "[gateway] event=after_request_open attempt=1",
        "[gateway] event=before_request_send attempt=1 payload_bytes=",
        "[gateway] event=after_request_send attempt=1 payload_bytes=",
        "[gateway] event=after_send attempt=1 payload_bytes=",
        "[gateway] event=before_receive_head attempt=1",
        "[gateway] event=after_receive_head attempt=1 status=200",
        "[gateway] event=resolved_model requested_model=openai/gpt-5.6-sol source=x-vercel-ai-gateway-model resolved_model=openai/",
        "[gateway] event=before_sse_consume attempt=1",
        "[sse] event type=response-metadata bytes=73 preview=<object_fields=2 values=[<string_bytes=17>,<string_bytes=32>]>",
        "[sse] event type=text-delta bytes=",
        "[sse] event type=error bytes=",
        "[sse] event type=finish bytes=52 preview=<object_fields=2 values=[<string_bytes=6>,<object_fields=1 values=[<string_bytes=5>]>]>",
        "[stream] termination cause=valid_finish finish_reason=error",
        "[gateway] event=sse_termination cause=valid_finish finish_reason=error",
        "[stream] sse summary events=4 finish_reason=error",
        "[gateway] event=after_sse_consume attempt=1 finish_reason=error content_bytes=7 tool_call_count=0",
        "[stream] completed attempt=1 finish_reason=error content_bytes=7 tool_calls=0",
        "[gateway] event=stream_complete attempt=1 finish_reason=error content_bytes=7 tool_call_count=0 tool_calls=0",
        "[gateway] event=after_receive_head attempt=1 status=401",
        "[stream] http status=401 attempt=1",
        "[stream] termination cause=done_without_finish finish_reason=(none)",
        "[stream] sse summary events=1 finish_reason=(none)",
        "[gateway] event=stream_complete attempt=1 finish_reason=(none) content_bytes=3",
        "[sse] event type=invalid bytes=10 preview=<invalid-json>",
        "[gateway] event=sse_consume_error attempt=1 err=InvalidGatewaySseEvent",
        "[sse] event=stream_state_anomaly reason=conflicting_start",
        "[sse] event=stream_state_anomaly reason=unmatched_or_late_delta",
        "[sse] event=stream_state_anomaly reason=unmatched_or_duplicate_end",
        "[sse] event=stream_state_anomaly reason=late_start",
        "[sse] event=tool_argument_integrity call_id=c2 tool_name=ask_user_question source=final_string bytes=30 failure=malformed_json diagnosis=syntax_error error_offset=1",
        "[gateway] event=stream_complete attempt=1 finish_reason=tool-calls content_bytes=0 tool_call_count=2 tool_calls=2",
    ];
    let mut from = 0;
    for needle in ordered {
        let found = position(&log[from..], needle);
        from += found + needle.len();
    }
    assert_eq!(log.matches("event=resolved_model").count(), 1);
    assert!(!log.contains("resolved_model_missing"));
}
