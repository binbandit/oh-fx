use std::borrow::Cow;
use std::fs;

use ofx_contract::{ProviderOptions, ToolCallId, ToolChoice};
use serde_json::Value;

use super::*;
use crate::test_sources::{CapturedImages, user_with_images};

const MODEL: &str = "test/model";
const USER_AGENT: &str = "oh-fx/1.2.3";
const DENIAL: &str = r#"{"error":{"type":"tool_permission_denied","reason":"policy_denied"}}"#;

fn request<'a>(
    model: &'a str,
    instructions: &'a [&'a str],
    messages: &'a [ChatMessage],
    tools: &'a [ToolSpec],
    provider_options: ProviderOptions<'a>,
) -> ModelRequest<'a> {
    ModelRequest {
        model,
        instructions,
        messages,
        tools,
        tool_choice: ToolChoice::Auto,
        max_output_tokens: None,
        provider_options,
        session_id: None,
    }
}

fn body(messages: &[ChatMessage]) -> Result<String> {
    build_request(
        &request(MODEL, &[], messages, &[], ProviderOptions::default()),
        USER_AGENT,
    )
}

fn parsed(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn prompt(text: &str) -> Vec<Value> {
    parsed(text)["prompt"].as_array().unwrap().clone()
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall::new(id, name, arguments)
}

fn provider_call(id: &str, name: &str, arguments: &str, result: &str) -> ToolCall {
    ToolCall {
        provider_result: Some(result.to_owned()),
        provenance: ToolExecutionProvenance::ProviderExecuted,
        ..call(id, name, arguments)
    }
}

fn assistant(content: Option<&str>, tool_calls: Vec<ToolCall>) -> ChatMessage {
    ChatMessage::Assistant {
        content: content.map(str::to_owned),
        tool_calls,
        provider_replay: None,
    }
}

fn replayed(
    content: Option<&str>,
    tool_calls: Vec<ToolCall>,
    model: &str,
    parts: &str,
) -> ChatMessage {
    ChatMessage::Assistant {
        content: content.map(str::to_owned),
        tool_calls,
        provider_replay: Some(ProviderReplay {
            source: replay_source(model),
            parts_json: parts.to_owned(),
        }),
    }
}

fn result(id: &str, name: &str, content: &str, status: ToolResultStatus) -> ChatMessage {
    ChatMessage::Tool {
        call_id: ToolCallId::new(id),
        tool_name: name.to_owned(),
        content: content.to_owned(),
        status,
    }
}

fn success(id: &str, name: &str, content: &str) -> ChatMessage {
    result(id, name, content, ToolResultStatus::Success)
}

fn tool(name: &str, description: &str, schema: &'static str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: Cow::Borrowed(schema),
    }
}

#[test]
fn required_requests_serialize_the_tool_choice_and_output_limit_exactly() {
    let messages = [ChatMessage::user("question")];
    let mut required = request(MODEL, &[], &messages, &[], ProviderOptions::default());
    required.tool_choice = ToolChoice::Required;
    required.max_output_tokens = Some(4096);
    assert_eq!(
        build_request(&required, USER_AGENT).unwrap(),
        r#"{"prompt":[{"role":"user","content":[{"type":"text","text":"question"}]}],"tools":[],"toolChoice":{"type":"required"},"maxOutputTokens":4096}"#
    );
}

#[test]
fn agent_turns_send_instructions_tools_the_output_limit_and_automatic_caching_exactly() {
    let messages = [ChatMessage::user("question")];
    let tools = [tool("read_file", "Read a file", r#"{"type":"object"}"#)];
    let options = ProviderOptions {
        prompt_caching: true,
        ..ProviderOptions::default()
    };
    let mut turn = request(MODEL, &["Be concise."], &messages, &tools, options);
    turn.max_output_tokens = Some(32_000);
    assert_eq!(
        build_request(&turn, USER_AGENT).unwrap(),
        r#"{"prompt":[{"role":"system","content":"Be concise."},{"role":"user","content":[{"type":"text","text":"question"}]}],"tools":[{"type":"function","name":"read_file","description":"Read a file","inputSchema":{"type":"object"}}],"toolChoice":{"type":"auto"},"maxOutputTokens":32000,"providerOptions":{"gateway":{"caching":"auto"}}}"#
    );
}

#[test]
fn the_default_reasoning_stays_silent_and_a_named_effort_is_provider_neutral() {
    let messages = [ChatMessage::user("question")];
    let silent = parsed(&body(&messages).unwrap());
    assert!(silent.get("reasoning").is_none());
    assert!(silent.get("providerOptions").is_none());
    assert!(silent.get("maxOutputTokens").is_none());
    let named = ProviderOptions {
        reasoning_effort: Some("future-tier"),
        ..ProviderOptions::default()
    };
    let named =
        parsed(&build_request(&request(MODEL, &[], &messages, &[], named), USER_AGENT).unwrap());
    assert_eq!(named["reasoning"], "future-tier");
    assert!(named.get("providerOptions").is_none());
}

#[test]
fn max_effort_is_sent_as_the_highest_accepted_tier() {
    let messages = [ChatMessage::user("question")];
    let options = ProviderOptions {
        reasoning_effort: Some("max"),
        ..ProviderOptions::default()
    };
    let sent =
        parsed(&build_request(&request(MODEL, &[], &messages, &[], options), USER_AGENT).unwrap());
    assert_eq!(sent["reasoning"], "xhigh");
}

#[test]
fn fast_mode_caching_and_xai_parallel_calls_share_one_provider_options_object() {
    let messages = [ChatMessage::user("question")];
    let fast = ProviderOptions {
        reasoning_effort: Some("future-tier"),
        fast: true,
        prompt_caching: false,
    };
    let sent =
        parsed(&build_request(&request(MODEL, &[], &messages, &[], fast), USER_AGENT).unwrap());
    assert_eq!(sent["reasoning"], "future-tier");
    assert!(sent.get("fast").is_none());
    assert_eq!(sent["providerOptions"]["gateway"]["speed"], "fast");
    let xai = build_request(
        &request("xai/grok-4", &[], &messages, &[], fast),
        USER_AGENT,
    )
    .unwrap();
    assert!(xai.contains(
        r#""providerOptions":{"gateway":{"speed":"fast"},"xai":{"parallelToolCalls":true}}"#
    ));
    let cached = ProviderOptions {
        prompt_caching: true,
        ..fast
    };
    let both = build_request(
        &request("xai/grok-4", &[], &messages, &[], cached),
        USER_AGENT,
    )
    .unwrap();
    assert!(both.contains(
        r#""providerOptions":{"gateway":{"speed":"fast","caching":"auto"},"xai":{"parallelToolCalls":true}}"#
    ));
    let plain = build_request(
        &request(
            "xai/grok-4",
            &[],
            &messages,
            &[],
            ProviderOptions::default(),
        ),
        USER_AGENT,
    )
    .unwrap();
    assert!(plain.contains(r#""providerOptions":{"xai":{"parallelToolCalls":true}}"#));
}

#[test]
fn automatic_caching_keeps_transient_context_and_grouped_tool_results() {
    let messages = [
        ChatMessage::user("read both"),
        assistant(
            None,
            vec![
                call("first", "read_file", "{}"),
                call("second", "read_file", "{}"),
            ],
        ),
        success("first", "read_file", "A"),
        success("second", "read_file", "B"),
    ];
    let cached = ProviderOptions {
        fast: true,
        prompt_caching: true,
        ..ProviderOptions::default()
    };
    let instructions = ["stable instructions", "runtime context"];
    let body = build_request(
        &request(MODEL, &instructions, &messages, &[], cached),
        USER_AGENT,
    )
    .unwrap();
    let sent = parsed(&body);
    assert_eq!(sent["providerOptions"]["gateway"]["caching"], "auto");
    assert_eq!(sent["providerOptions"]["gateway"]["speed"], "fast");
    assert!(!body.contains("cacheControl"));
    let entries = prompt(&body);
    assert_eq!(entries[1]["content"], "runtime context");
    let results = entries[4]["content"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["output"]["value"], "A");
    assert_eq!(results[1]["output"]["value"], "B");
    let uncached = ProviderOptions {
        prompt_caching: false,
        ..cached
    };
    let default_body = build_request(
        &request(MODEL, &instructions, &messages, &[], uncached),
        USER_AGENT,
    )
    .unwrap();
    assert_eq!(default_body, body.replace(r#","caching":"auto""#, ""));
}

#[test]
fn one_shot_requests_leave_caching_to_the_provider() {
    let messages = [ChatMessage::user("question")];
    let sent = build_request(
        &request(
            MODEL,
            &["system prompt"],
            &messages,
            &[],
            ProviderOptions::default(),
        ),
        USER_AGENT,
    )
    .unwrap();
    assert!(!sent.contains("cacheControl"));
    assert!(!sent.contains("providerOptions"));
}

#[test]
fn assistant_tool_call_input_is_raw_json() {
    let messages = [
        assistant(
            Some("I will read it"),
            vec![call("call_1", "read_file", r#"{"path":"src/main.zig"}"#)],
        ),
        success("call_1", "read_file", "contents"),
    ];
    let sent = body(&messages).unwrap();
    assert!(sent.contains(r#"{"role":"assistant","content":[{"type":"text","text":"I will read it"},{"type":"tool-call","toolCallId":"call_1","toolName":"read_file","input":{"path":"src/main.zig"}}]}"#));
}

#[test]
fn tool_results_escape_their_output() {
    let messages = [
        assistant(None, vec![call("call_1", "terminal", "{}")]),
        success("call_1", "terminal", "line\n\ttext"),
    ];
    let sent = body(&messages).unwrap();
    assert!(sent.contains(r#"{"role":"tool","content":[{"type":"tool-result","toolCallId":"call_1","toolName":"terminal","output":{"type":"text","value":"line\n\ttext"}}]}"#));
}

#[test]
fn tool_result_status_selects_the_vercel_output_variant() {
    let review_hold = r#"{"error":{"type":"tool_review_held","reason":"review_caution"}}"#;
    let malformed_denial =
        r#"{"error":{"type":"tool_permission_denied","reason":"review_caution"}}"#;
    let cases = [
        (
            ToolResultStatus::Success,
            "successful result",
            "text",
            "value",
        ),
        (
            ToolResultStatus::Failure,
            "ordinary failure",
            "error-text",
            "value",
        ),
        (
            ToolResultStatus::Failure,
            DENIAL,
            "execution-denied",
            "reason",
        ),
        (
            ToolResultStatus::Failure,
            review_hold,
            "execution-denied",
            "reason",
        ),
        (ToolResultStatus::Success, DENIAL, "text", "value"),
        (
            ToolResultStatus::Failure,
            malformed_denial,
            "error-text",
            "value",
        ),
    ];
    for (status, content, output_type, field) in cases {
        let messages = [
            assistant(None, vec![call("call_1", "terminal", "{}")]),
            result("call_1", "terminal", content, status),
        ];
        let entries = prompt(&body(&messages).unwrap());
        let part = &entries[1]["content"][0];
        assert_eq!(part["toolCallId"], "call_1");
        assert_eq!(part["toolName"], "terminal");
        let output = part["output"].as_object().unwrap();
        assert_eq!(output["type"], output_type, "{content}");
        assert_eq!(output[field], content);
        let absent = if field == "value" { "reason" } else { "value" };
        assert!(output.get(absent).is_none());
    }
}

#[test]
fn each_tool_result_group_stays_together() {
    let calls = vec![
        call("call_a", "read_a", "{}"),
        call("call_b", "read_b", "{}"),
        call("call_c", "read_c", "{}"),
    ];
    let messages = [
        ChatMessage::user("read all three"),
        assistant(None, calls.clone()),
        success("call_a", "read_a", "A\n\"quoted\""),
        result("call_b", "read_b", "failed", ToolResultStatus::Failure),
        result("call_c", "read_c", DENIAL, ToolResultStatus::Failure),
        ChatMessage::user("try again"),
        assistant(None, calls[..1].to_vec()),
        success("call_a", "read_a", ""),
        assistant(Some("done"), Vec::new()),
    ];
    let sent = body(&messages).unwrap();
    let entries = prompt(&sent);
    let roles: Vec<&str> = entries
        .iter()
        .map(|entry| entry["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        [
            "user",
            "assistant",
            "tool",
            "user",
            "assistant",
            "tool",
            "assistant"
        ]
    );
    let results = entries[2]["content"].as_array().unwrap();
    let expected = [
        ("call_a", "read_a", "text", "A\n\"quoted\""),
        ("call_b", "read_b", "error-text", "failed"),
        ("call_c", "read_c", "execution-denied", DENIAL),
    ];
    for (part, (id, name, output_type, value)) in results.iter().zip(expected) {
        assert_eq!(part["type"], "tool-result");
        assert_eq!(part["toolCallId"], id);
        assert_eq!(part["toolName"], name);
        assert_eq!(part["output"]["type"], output_type);
        let shown = part["output"]
            .get("value")
            .or_else(|| part["output"].get("reason"));
        assert_eq!(shown.unwrap(), value);
    }
    assert_eq!(entries[5]["content"].as_array().unwrap().len(), 1);
    assert_eq!(entries[5]["content"][0]["output"]["value"], "");
    assert_eq!(body(&messages).unwrap(), sent);
}

#[test]
fn history_validation_accepts_paired_calls_and_out_of_order_results() {
    let paired = [
        ChatMessage::user("read it"),
        assistant(
            None,
            vec![call("call_1", "read_file", r#"{"path":"src/main.zig"}"#)],
        ),
        success("call_1", "read_file", "contents"),
        assistant(Some("done"), Vec::new()),
    ];
    assert!(body(&paired).unwrap().contains(r#""toolCallId":"call_1""#));
    let reordered = [
        assistant(
            None,
            vec![
                call("call_1", "read_file", r#"{"path":"a.txt"}"#),
                call("call_2", "glob_files", r#"{"pattern":"*"}"#),
            ],
        ),
        success("call_2", "glob_files", "second"),
        success("call_1", "read_file", "first"),
    ];
    let sent = body(&reordered).unwrap();
    assert!(sent.contains(r#""toolCallId":"call_2""#) && sent.contains(r#""toolCallId":"call_1""#));
}

#[test]
fn history_validation_rejects_every_unpaired_or_malformed_tool_step() {
    let read = |arguments: &str| vec![call("call_1", "read_file", arguments)];
    let histories = [
        vec![
            ChatMessage::user("hello"),
            success("call_1", "read_file", "orphan"),
        ],
        vec![
            assistant(None, read("{not json")),
            success("call_1", "read_file", "contents"),
        ],
        vec![
            assistant(None, read(r#"{"depth":1,"depth":2}"#)),
            success("call_1", "read_file", "contents"),
        ],
        vec![
            assistant(None, read("{}")),
            assistant(Some("next"), Vec::new()),
        ],
        vec![
            assistant(
                None,
                vec![
                    call("call_1", "read_file", "{}"),
                    call("call_2", "glob_files", "{}"),
                ],
            ),
            success("call_1", "read_file", "first"),
            success("call_1", "read_file", "duplicate"),
        ],
        vec![
            assistant(None, read("{}")),
            success("call_1", "write_file", "contents"),
        ],
        vec![
            assistant(
                None,
                vec![
                    call("same", "read_file", "{}"),
                    call("same", "glob_files", "{}"),
                ],
            ),
            success("same", "read_file", "a"),
            success("same", "glob_files", "b"),
        ],
        vec![
            assistant(None, read("")),
            success("call_1", "read_file", "contents"),
        ],
    ];
    for history in histories {
        assert_eq!(
            body(&history),
            Err(RequestError::InvalidGatewayHistory),
            "{history:?}"
        );
    }
}

#[test]
fn non_object_function_arguments_cannot_enter_a_request() {
    for arguments in ["[]", "42", "null", "true", "\"text\""] {
        let messages = [
            ChatMessage::user("read"),
            assistant(None, vec![call("call", "read_file", arguments)]),
            result(
                "call",
                "read_file",
                "not executed",
                ToolResultStatus::Failure,
            ),
        ];
        assert_eq!(
            body(&messages),
            Err(RequestError::InvalidGatewayHistory),
            "{arguments}"
        );
    }
}

#[test]
fn provider_owned_calls_keep_their_ids_and_non_object_input() {
    let messages = [
        assistant(
            None,
            vec![provider_call("native:0", "native", "[]", "result")],
        ),
        success("native:0", "native", "result"),
    ];
    let sent = body(&messages).unwrap();
    assert!(sent.contains(r#""toolCallId":"native:0""#));
    assert!(sent.contains(r#""input":[]"#));
}

#[test]
fn nonportable_call_ids_are_projected_without_changing_the_history() {
    let source_id = "functions.read_file:0";
    let second_id = "functions/read_file:0";
    let messages = [
        assistant(
            None,
            vec![
                call(source_id, "read_file", "{}"),
                call(second_id, "read_file", "{}"),
            ],
        ),
        success(second_id, "read_file", "second"),
        success(source_id, "read_file", "result"),
    ];
    let entries = prompt(&body(&messages).unwrap());
    let call_id = entries[0]["content"][0]["toolCallId"].as_str().unwrap();
    let second_call_id = entries[0]["content"][1]["toolCallId"].as_str().unwrap();
    let results = entries[1]["content"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_ne!(call_id, source_id);
    assert!(call_id.len() <= 64);
    assert!(
        call_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    );
    assert_eq!(results[1]["toolCallId"], call_id);
    assert_ne!(call_id, second_call_id);
    assert_eq!(results[0]["toolCallId"], second_call_id);
    let ChatMessage::Assistant { tool_calls, .. } = &messages[0] else {
        unreachable!();
    };
    assert_eq!(tool_calls[0].id.as_str(), source_id);
}

#[test]
fn system_messages_in_the_conversation_are_refused_as_upstream_refuses_them() {
    let messages = [
        ChatMessage::user("question"),
        ChatMessage::System {
            content: "late instruction".to_owned(),
        },
    ];
    assert_eq!(body(&messages), Err(RequestError::InvalidProviderPrompt));
}

#[test]
fn replays_restore_reasoning_and_call_metadata_without_changing_the_input() {
    let parts = r#"[{"type":"reasoning","text":"","providerOptions":{"openai":{"reasoningEncryptedContent":"opaque"}}},{"type":"text","offset":0,"length":7},{"type":"tool-call","toolCallId":"call-1","providerOptions":{"vertex":{"thoughtSignature":"signature"}}}]"#;
    let messages = [
        replayed(
            Some("visible"),
            vec![call("call-1", "read_file", r#"{"path":"file"}"#)],
            MODEL,
            parts,
        ),
        success("call-1", "read_file", "result"),
    ];
    let sent = body(&messages).unwrap();
    let entries = prompt(&sent);
    let content = entries[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["type"], "reasoning");
    assert_eq!(
        content[0]["providerOptions"]["openai"]["reasoningEncryptedContent"],
        "opaque"
    );
    assert_eq!(content[1]["text"], "visible");
    assert_eq!(content[2]["toolName"], "read_file");
    assert_eq!(content[2]["input"]["path"], "file");
    assert_eq!(
        content[2]["providerOptions"]["vertex"]["thoughtSignature"],
        "signature"
    );
    assert!(sent.contains(r#"{"type":"reasoning","text":"","providerOptions":{"openai":{"reasoningEncryptedContent":"opaque"}}}"#));
}

#[test]
fn replays_append_text_and_calls_they_do_not_cover() {
    let parts =
        r#"[{"type":"text","offset":0,"length":4,"providerOptions":{"openai":{"itemId":"msg"}}}]"#;
    let messages = [
        replayed(
            Some("done and more"),
            vec![call("call-1", "read_file", "{}")],
            MODEL,
            parts,
        ),
        success("call-1", "read_file", "result"),
    ];
    let entries = prompt(&body(&messages).unwrap());
    let content = entries[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["text"], "done");
    assert_eq!(content[0]["providerOptions"]["openai"]["itemId"], "msg");
    assert_eq!(content[1]["text"], " and more");
    assert!(content[1].get("providerOptions").is_none());
    assert_eq!(content[2]["toolCallId"], "call-1");
}

#[test]
fn replays_from_another_route_are_left_out() {
    let parts = r#"[{"type":"reasoning","text":"secret"}]"#;
    let messages = [replayed(Some("visible"), Vec::new(), "other/model", parts)];
    let entries = prompt(&body(&messages).unwrap());
    assert_eq!(
        entries[0],
        parsed(r#"{"role":"assistant","content":[{"type":"text","text":"visible"}]}"#)
    );
}

#[test]
fn malformed_replays_are_refused_as_invalid_provider_state() {
    let cases = [
        "{}",
        "[1]",
        r#"[{"text":"missing type"}]"#,
        r#"[{"type":"image"}]"#,
        r#"[{"type":"reasoning"}]"#,
        r#"[{"type":"text","offset":1,"length":1}]"#,
        r#"[{"type":"text","offset":0,"length":99}]"#,
        r#"[{"type":"text","offset":0.0,"length":1}]"#,
        r#"[{"type":"text","offset":-1,"length":1}]"#,
        r#"[{"type":"tool-call","toolCallId":"missing"}]"#,
        r#"[{"type":"tool-call","toolCallId":"call"},{"type":"tool-call","toolCallId":"call"}]"#,
        r#"[{"type":"reasoning","text":"","providerOptions":[]}]"#,
        r#"[{"type":"reasoning","text":"","providerOptions":{"openai":"flat"}}]"#,
        r#"[{"type":"reasoning","text":"a","text":"b"}]"#,
        "not json",
    ];
    for parts in cases {
        let messages = [
            replayed(
                Some("text"),
                vec![call("call", "read_file", "{}")],
                MODEL,
                parts,
            ),
            success("call", "read_file", "result"),
        ];
        assert_eq!(
            body(&messages),
            Err(RequestError::InvalidProviderState),
            "{parts}"
        );
    }
    let oversized = format!("[{}]", " ".repeat(MAX_REPLAY_BYTES));
    let messages = [replayed(Some("text"), Vec::new(), MODEL, &oversized)];
    assert_eq!(body(&messages), Err(RequestError::ProviderStateTooLarge));
}

#[test]
fn tools_use_the_flattened_function_envelope_with_capped_descriptions() {
    let messages = [ChatMessage::user("question")];
    let long = "d".repeat(2000);
    let tools = [
        tool(
            "mcp_fs_read",
            "Read",
            r#"{"type":"object","properties":{}}"#,
        ),
        tool("long", &long, "{}"),
        tool("mcp_fs_read", "Duplicate", "{}"),
    ];
    let sent = parsed(
        &build_request(
            &request(MODEL, &[], &messages, &tools, ProviderOptions::default()),
            USER_AGENT,
        )
        .unwrap(),
    );
    let listed = sent["tools"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed[0],
        parsed(
            r#"{"type":"function","name":"mcp_fs_read","description":"Read","inputSchema":{"type":"object","properties":{}}}"#
        )
    );
    let description = listed[1]["description"].as_str().unwrap();
    assert_eq!(description.len(), 1024);
    assert!(description.ends_with("... [truncated]"));
}

#[test]
fn only_glm_5_2_names_the_product_user_agent_in_the_body() {
    let messages = [ChatMessage::user("question")];
    for (model, named) in [("zai/glm-5.2", true), ("poolside/laguna-s-2.1-free", false)] {
        let sent = build_request(
            &request(model, &[], &messages, &[], ProviderOptions::default()),
            USER_AGENT,
        )
        .unwrap();
        let headers = parsed(&sent).get("headers").cloned();
        if named {
            assert!(sent.ends_with(r#","headers":{"user-agent":"oh-fx/1.2.3"}}"#));
        } else {
            assert_eq!(headers, None);
        }
    }
}

#[test]
fn tool_choice_none_and_session_ids_do_not_change_the_body_shape() {
    let messages = [ChatMessage::user("question")];
    let mut none = request(MODEL, &[], &messages, &[], ProviderOptions::default());
    none.tool_choice = ToolChoice::None;
    none.session_id = Some("session");
    assert_eq!(
        build_request(&none, USER_AGENT).unwrap(),
        r#"{"prompt":[{"role":"user","content":[{"type":"text","text":"question"}]}],"tools":[],"toolChoice":{"type":"none"}}"#
    );
}

#[test]
fn empty_user_text_sends_no_text_part() {
    let messages = [ChatMessage::user(""), assistant(Some(""), Vec::new())];
    let entries = prompt(&body(&messages).unwrap());
    assert_eq!(entries[0], parsed(r#"{"role":"user","content":[]}"#));
    assert_eq!(entries[1], parsed(r#"{"role":"assistant","content":[]}"#));
}

#[test]
fn user_images_follow_their_text_as_file_parts() {
    let images = CapturedImages::new();
    let image = images.capture(1, "image.png", b"\x89PNG\r\n\x1a\nabc");
    let messages = [
        user_with_images("look", vec![image.clone()]),
        assistant(Some("seen"), Vec::new()),
        user_with_images("", vec![image.clone(), image]),
    ];
    let sent = body(&messages).unwrap();
    assert!(sent.contains(
        r#"{"role":"user","content":[{"type":"text","text":"look"},{"type":"file","mediaType":"image/png","data":{"type":"data","data":"iVBORw0KGgphYmM="}}]}"#
    ));
    let entries = prompt(&sent);
    let parts: Vec<&str> = entries[2]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part["type"].as_str().unwrap())
        .collect();
    assert_eq!(parts, ["file", "file"]);
}

#[test]
fn user_images_use_their_captured_bytes_and_refuse_unavailable_snapshots() {
    let images = CapturedImages::new();
    let attachment = images.capture(1, "source.png", b"\x89PNG\r\n\x1a\nA");
    let messages = [user_with_images("", vec![attachment.clone()])];
    fs::remove_file(&attachment.path).unwrap();
    assert!(
        body(&messages)
            .unwrap()
            .contains(r#""data":"iVBORw0KGgpB""#)
    );
    let snapshot = attachment.snapshot_path.as_deref().unwrap();
    fs::write(snapshot, b"\x89PNG\r\n\x1a\nB").unwrap();
    assert_eq!(
        body(&messages),
        Err(RequestError::Image(AttachmentError::ImageSnapshotCorrupt))
    );
    fs::remove_file(snapshot).unwrap();
    assert_eq!(
        body(&messages),
        Err(RequestError::Image(AttachmentError::FileNotFound))
    );
    assert_eq!(
        RequestError::Image(AttachmentError::FileNotFound).to_string(),
        "FileNotFound"
    );
}
