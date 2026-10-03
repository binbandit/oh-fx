use ofx_contract::{
    ProviderOptions, ProviderReplay, ReplaySource, ToolCallId, ToolResultStatus, ToolSpec,
};
use serde_json::{Value, json};

use super::*;

const TEST_STOP: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
const TEST_TOOLS_FINISH: &str =
    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
const TEST_TEXT: &str = r#"{"id":"chat-1","model":"resolved-model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}"#;
const TEST_CALL: &str = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"x\"}"}}]}}]}"#;

struct OwnedRequest {
    model: String,
    instructions: Vec<&'static str>,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolSpec>,
    tool_choice: ToolChoice,
    max_output_tokens: Option<u32>,
}

impl OwnedRequest {
    fn borrowed(&self) -> ModelRequest<'_> {
        ModelRequest {
            model: &self.model,
            instructions: &self.instructions,
            messages: &self.messages,
            tools: &self.tools,
            tool_choice: self.tool_choice,
            max_output_tokens: self.max_output_tokens,
            provider_options: ProviderOptions::default(),
            session_id: None,
        }
    }
}

fn test_request() -> OwnedRequest {
    OwnedRequest {
        model: "opaque/local-model:8b".to_owned(),
        instructions: vec!["first", "second"],
        messages: vec![ChatMessage::user("hi")],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        max_output_tokens: None,
    }
}

fn tool(name: &str, description: &str, schema: &'static str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: schema,
    }
}

fn test_tools() -> Vec<ToolSpec> {
    vec![
        tool(
            "read_file",
            "Read a file.",
            r#"{"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false,"required":["path"]}"#,
        ),
        tool(
            "shell",
            "Run a command.",
            r#"{"type":"object","properties":{"request":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}"#,
        ),
    ]
}

fn test_tool_request() -> OwnedRequest {
    OwnedRequest {
        tools: test_tools(),
        ..test_request()
    }
}

fn options(mode: ToolChoiceMode) -> RequestOptions {
    RequestOptions {
        tool_choice_mode: mode,
        max_tokens_parameter: MaxTokensParameter::MaxTokens,
    }
}

fn build(request: &OwnedRequest, mode: ToolChoiceMode) -> ProtocolResult<String> {
    build_request(&request.borrowed(), options(mode))
        .map(|prepared| String::from_utf8(prepared.body).unwrap())
}

fn selection(request: &OwnedRequest) -> Selection {
    build_request(&request.borrowed(), options(ToolChoiceMode::Omit))
        .unwrap()
        .selection
}

fn new_reducer(request: &OwnedRequest) -> Reducer {
    Reducer::new(selection(request), Limits::default())
}

fn accept(reducer: &mut Reducer, data: &str) -> ProtocolResult<()> {
    reducer.accept(data.as_bytes(), false).map(|_| ())
}

fn finish_with(reducer: &mut Reducer, terminal: &str) -> ProtocolResult<Completion> {
    accept(reducer, terminal)?;
    accept(reducer, "[DONE]")?;
    reducer.finish(false)
}

fn usage_snapshot(reducer: &mut Reducer, usage: &str) -> ProtocolResult<()> {
    let choices = if reducer.phase == Phase::Finished {
        "[]"
    } else {
        r#"[{"index":0,"delta":{}}]"#
    };
    accept(
        reducer,
        &format!(r#"{{"choices":{choices},"usage":{usage}}}"#),
    )
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall::new(id, name, arguments)
}

fn tool_result(id: &str, name: &str, content: &str) -> ChatMessage {
    ChatMessage::Tool {
        call_id: ToolCallId::new(id),
        tool_name: name.to_owned(),
        content: content.to_owned(),
        status: ToolResultStatus::Success,
    }
}

#[test]
fn chat_completions_error_masking_survives_json_and_recovery_decoding() {
    let cases = [
        (
            r#"{"error":{"message":"rejected alpha\/beta","code":"alpha\/beta"}}"#,
            "alpha/beta",
        ),
        (r#"{"error":{"message":"rejected alpha"}}"#, "alpha"),
        (
            r#"{"error":{"message":"rejected alpha\"beta"}}"#,
            "alpha\"beta",
        ),
        ("rejected alpha/beta", "alpha/beta"),
    ];
    for (raw, key) in cases {
        let detail = redact_error_detail(raw.as_bytes(), &[key.to_owned()]);
        assert!(!detail.contains(key), "{detail}");
        let escaped = Value::String(key.to_owned()).to_string();
        assert!(!detail.contains(&escaped[1..escaped.len() - 1]), "{detail}");
        assert!(detail.contains("rejected"), "{detail}");
    }
    let duplicate = redact_error_detail(
        br#"{"error":{"message":"alpha\/beta"},"alpha/beta":0,"alpha\/beta":1}"#,
        &["alpha/beta".to_owned()],
    );
    assert_eq!(duplicate, DETAIL_DECODE_NOTICE);
    let nested = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    assert_eq!(
        redact_error_detail(nested.as_bytes(), &["alpha".to_owned()]),
        DETAIL_NESTING_NOTICE
    );
    assert_eq!(
        redact_error_detail(&vec![b'x'; MAX_ERROR_DETAIL_BYTES + 1], &[]),
        DETAIL_LIMIT_NOTICE
    );
}

#[test]
fn chat_completions_emits_reasoning_fields_as_deltas_before_content() {
    for field in REASONING_FIELDS {
        let mut reducer = new_reducer(&test_request());
        let mut reasoning = Vec::new();
        for fragment in ["think ", "carefully"] {
            let chunk =
                format!(r#"{{"choices":[{{"index":0,"delta":{{"{field}":"{fragment}"}}}}]}}"#);
            reasoning.extend(reducer.accept(chunk.as_bytes(), false).unwrap().reasoning);
        }
        assert_eq!(reasoning.concat(), "think carefully");
        accept(&mut reducer, TEST_TEXT).unwrap();
        let completion = finish_with(&mut reducer, TEST_STOP).unwrap();
        assert_eq!(completion.content.as_deref(), Some("hello"));
    }
}

#[test]
fn chat_completions_reasoning_deltas_own_decoded_text_and_skip_null_and_empty_aliases() {
    let mut reducer = new_reducer(&test_request());
    let silent = reducer
        .accept(
            br#"{"choices":[{"index":0,"delta":{"reasoning":null,"reasoning_content":"","reasoning_details":null}}]}"#,
            false,
        )
        .unwrap();
    assert_eq!(silent, Deltas::default());
    let deltas = reducer
        .accept(
            br#"{"choices":[{"index":0,"delta":{"reasoning":"think\n","reasoning_content":"other\t","content":"answer"}}]}"#,
            false,
        )
        .unwrap();
    assert_eq!(deltas.reasoning, ["think\n", "other\t"]);
    assert_eq!(deltas.content.as_deref(), Some("answer"));
    let completion = finish_with(&mut reducer, TEST_STOP).unwrap();
    assert_eq!(completion.content.as_deref(), Some("answer"));
}

#[test]
fn chat_completions_reasoning_rejects_malformed_deltas() {
    for field in [
        r#""reasoning":{}"#,
        r#""reasoning_content":1"#,
        r#""reasoning_details":{}"#,
        r#""reasoning_details":[null]"#,
        r#""reasoning_details":[[]]"#,
    ] {
        let mut reducer = new_reducer(&test_request());
        let chunk = format!(r#"{{"choices":[{{"index":0,"delta":{{{field}}}}}]}}"#);
        assert_eq!(
            accept(&mut reducer, &chunk),
            Err(ProtocolError::InvalidChunk)
        );
        assert_eq!(reducer.finish(false), Err(ProtocolError::StreamClosed));
    }
}

#[test]
fn chat_completions_cumulative_usage_advances_to_final_counts_without_double_counting() {
    let mut reducer = new_reducer(&test_request());
    for chunk in [
        r#"{"choices":[{"index":0,"delta":{"content":"hello"}}],"usage":{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}}"#,
        r#"{"choices":[{"index":0,"delta":{"content":" world"}}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        "[DONE]",
    ] {
        accept(&mut reducer, chunk).unwrap();
    }
    let completion = reducer.finish(false).unwrap();
    assert_eq!(completion.usage.input_tokens, Some(10));
    assert_eq!(completion.usage.output_tokens, Some(3));
    assert_eq!(completion.content.as_deref(), Some("hello world"));
}

#[test]
fn chat_completions_progress_usage_can_finalize_in_a_trailer_but_cannot_decrease_or_contradict_finals()
 {
    let progress = r#"{"choices":[{"index":0,"delta":{}}],"usage":{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}}"#;
    let mut reducer = new_reducer(&test_request());
    accept(&mut reducer, progress).unwrap();
    accept(&mut reducer, progress).unwrap();
    accept(&mut reducer, TEST_STOP).unwrap();
    let trailer =
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#;
    accept(&mut reducer, trailer).unwrap();
    accept(&mut reducer, trailer).unwrap();
    accept(&mut reducer, "[DONE]").unwrap();
    assert_eq!(reducer.finish(false).unwrap().usage.output_tokens, Some(3));
    let cases = [
        (
            r#"{"prompt_tokens":9,"completion_tokens":2,"total_tokens":11}"#,
            ProtocolError::ConflictingIdentity,
        ),
        (
            r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10}"#,
            ProtocolError::ConflictingIdentity,
        ),
        (
            r#"{"prompt_tokens":10,"completion_tokens":-1}"#,
            ProtocolError::InvalidChunk,
        ),
        (
            r#"{"prompt_tokens":10,"total_tokens":9}"#,
            ProtocolError::ConflictingIdentity,
        ),
    ];
    for (usage, failure) in cases {
        for is_final in [false, true] {
            let mut invalid = new_reducer(&test_request());
            accept(&mut invalid, progress).unwrap();
            if is_final {
                accept(&mut invalid, TEST_STOP).unwrap();
            }
            let choices = if is_final {
                "[]"
            } else {
                r#"[{"index":0,"delta":{}}]"#
            };
            let chunk = format!(r#"{{"choices":{choices},"usage":{usage}}}"#);
            assert_eq!(accept(&mut invalid, &chunk), Err(failure), "{usage}");
        }
    }
    let mut totals = new_reducer(&test_request());
    accept(
        &mut totals,
        r#"{"choices":[{"index":0,"delta":{}}],"usage":{"total_tokens":10}}"#,
    )
    .unwrap();
    accept(&mut totals, TEST_STOP).unwrap();
    accept(&mut totals, r#"{"choices":[],"usage":{"total_tokens":11}}"#).unwrap();
    assert_eq!(
        accept(&mut totals, r#"{"choices":[],"usage":{"total_tokens":12}}"#),
        Err(ProtocolError::ConflictingIdentity)
    );
}

#[test]
fn chat_completions_accepts_usage_totals_that_do_not_add_up() {
    let mut reducer = new_reducer(&test_request());
    accept(&mut reducer, TEST_TEXT).unwrap();
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":42}}"#,
    )
    .unwrap();
    accept(&mut reducer, "[DONE]").unwrap();
    let completion = reducer.finish(false).unwrap();
    assert_eq!(completion.usage.input_tokens, Some(10));
    assert_eq!(completion.usage.output_tokens, Some(3));
}

#[test]
fn chat_completions_final_usage_allows_optional_total_metadata_in_either_order() {
    let counts = r#"{"prompt_tokens":10,"completion_tokens":3}"#;
    let with_total = r#"{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}"#;
    for total_first in [false, true] {
        let mut reducer = new_reducer(&test_request());
        let terminal = format!(
            r#"{{"choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}],"usage":{}}}"#,
            if total_first { with_total } else { counts }
        );
        accept(&mut reducer, &terminal).unwrap();
        usage_snapshot(&mut reducer, if total_first { counts } else { with_total }).unwrap();
        usage_snapshot(&mut reducer, "{}").unwrap();
        usage_snapshot(
            &mut reducer,
            r#"{"prompt_tokens":null,"completion_tokens":null,"total_tokens":null}"#,
        )
        .unwrap();
        assert_eq!(reducer.usage.total, Some(13));
        accept(&mut reducer, "[DONE]").unwrap();
        let completion = reducer.finish(false).unwrap();
        assert_eq!(completion.usage.input_tokens, Some(10));
        assert_eq!(completion.usage.output_tokens, Some(3));
    }
}

#[test]
fn chat_completions_partial_usage_snapshots_enrich_without_erasing_known_counters() {
    for is_final in [false, true] {
        let mut reducer = new_reducer(&test_request());
        if is_final {
            accept(&mut reducer, TEST_STOP).unwrap();
        }
        usage_snapshot(&mut reducer, r#"{"total_tokens":13}"#).unwrap();
        usage_snapshot(&mut reducer, r#"{"prompt_tokens":10}"#).unwrap();
        assert_eq!(reducer.usage.output, None);
        usage_snapshot(&mut reducer, r#"{"completion_tokens":3}"#).unwrap();
        usage_snapshot(&mut reducer, r#"{"total_tokens":null}"#).unwrap();
        assert_eq!(reducer.usage.input, Some(10));
        assert_eq!(reducer.usage.output, Some(3));
        assert_eq!(reducer.usage.total, Some(13));
        if !is_final {
            accept(&mut reducer, TEST_STOP).unwrap();
        }
        usage_snapshot(&mut reducer, "{}").unwrap();
        accept(&mut reducer, "[DONE]").unwrap();
        let completion = reducer.finish(false).unwrap();
        assert_eq!(completion.usage.input_tokens, Some(10));
        assert_eq!(completion.usage.output_tokens, Some(3));
    }
    let mut progress = new_reducer(&test_request());
    usage_snapshot(
        &mut progress,
        r#"{"prompt_tokens":10,"completion_tokens":1}"#,
    )
    .unwrap();
    usage_snapshot(&mut progress, r#"{"completion_tokens":2}"#).unwrap();
    accept(&mut progress, TEST_STOP).unwrap();
    usage_snapshot(
        &mut progress,
        r#"{"completion_tokens":3,"total_tokens":13}"#,
    )
    .unwrap();
    assert_eq!(progress.usage.input, Some(10));
    assert_eq!(progress.usage.output, Some(3));
    assert_eq!(progress.usage.total, Some(13));
}

#[test]
fn chat_completions_progress_with_omitted_totals_reaches_enriched_final_usage() {
    let mut reducer = new_reducer(&test_request());
    usage_snapshot(
        &mut reducer,
        r#"{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}"#,
    )
    .unwrap();
    usage_snapshot(
        &mut reducer,
        r#"{"prompt_tokens":10,"completion_tokens":2}"#,
    )
    .unwrap();
    assert_eq!(reducer.usage.total, Some(11));
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#,
    )
    .unwrap();
    usage_snapshot(&mut reducer, r#"{"total_tokens":13}"#).unwrap();
    usage_snapshot(&mut reducer, "{}").unwrap();
    assert_eq!(reducer.usage.total, Some(13));
    accept(&mut reducer, "[DONE]").unwrap();
    let completion = reducer.finish(false).unwrap();
    assert_eq!(completion.usage.input_tokens, Some(10));
    assert_eq!(completion.usage.output_tokens, Some(3));
}

#[test]
fn chat_completions_partial_final_fields_do_not_freeze_carried_progress_observations() {
    for initial_final in [
        r#"{"prompt_tokens":10,"total_tokens":13}"#,
        "{}",
        r#"{"prompt_tokens":null,"completion_tokens":null,"total_tokens":null}"#,
    ] {
        let mut reducer = new_reducer(&test_request());
        usage_snapshot(
            &mut reducer,
            r#"{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}"#,
        )
        .unwrap();
        accept(&mut reducer, TEST_STOP).unwrap();
        usage_snapshot(&mut reducer, initial_final).unwrap();
        usage_snapshot(&mut reducer, r#"{"completion_tokens":3}"#).unwrap();
        usage_snapshot(&mut reducer, r#"{"prompt_tokens":10,"total_tokens":13}"#).unwrap();
        assert_eq!(reducer.usage.input, Some(10));
        assert_eq!(reducer.usage.output, Some(3));
        assert_eq!(reducer.usage.total, Some(13));
        assert_eq!(
            usage_snapshot(
                &mut reducer,
                r#"{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}"#
            ),
            Err(ProtocolError::ConflictingIdentity)
        );
    }
}

#[test]
fn chat_completions_final_output_alone_permits_later_final_input_enrichment() {
    let mut reducer = new_reducer(&test_request());
    usage_snapshot(
        &mut reducer,
        r#"{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}"#,
    )
    .unwrap();
    accept(&mut reducer, TEST_STOP).unwrap();
    usage_snapshot(&mut reducer, r#"{"completion_tokens":3}"#).unwrap();
    usage_snapshot(&mut reducer, r#"{"prompt_tokens":11,"total_tokens":14}"#).unwrap();
    assert_eq!(reducer.usage.input, Some(11));
    assert_eq!(reducer.usage.output, Some(3));
    assert_eq!(reducer.usage.total, Some(14));
    assert_eq!(
        usage_snapshot(&mut reducer, r#"{"prompt_tokens":12,"total_tokens":15}"#),
        Err(ProtocolError::ConflictingIdentity)
    );
}

#[test]
fn chat_completions_progress_totals_constrain_current_assertions_rather_than_stale_equality() {
    let mut early = new_reducer(&test_request());
    usage_snapshot(&mut early, r#"{"total_tokens":13}"#).unwrap();
    usage_snapshot(&mut early, r#"{"prompt_tokens":14}"#).unwrap();
    assert_eq!(early.usage.total, Some(13));
    assert_eq!(early.usage.input, Some(14));
    let mut reducer = new_reducer(&test_request());
    usage_snapshot(
        &mut reducer,
        r#"{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}"#,
    )
    .unwrap();
    usage_snapshot(&mut reducer, r#"{"completion_tokens":2,"total_tokens":15}"#).unwrap();
    assert_eq!(reducer.usage.input, Some(10));
    assert_eq!(reducer.usage.output, Some(2));
    usage_snapshot(&mut reducer, r#"{"prompt_tokens":13}"#).unwrap();
    accept(&mut reducer, TEST_STOP).unwrap();
    usage_snapshot(
        &mut reducer,
        r#"{"prompt_tokens":13,"completion_tokens":2,"total_tokens":15}"#,
    )
    .unwrap();
    accept(&mut reducer, "[DONE]").unwrap();
    let completion = reducer.finish(false).unwrap();
    assert_eq!(completion.usage.input_tokens, Some(13));
    assert_eq!(completion.usage.output_tokens, Some(2));
}

fn usage_follow_up(first: &str, next: &str, is_final: bool) -> (Reducer, ProtocolResult<()>) {
    let mut reducer = new_reducer(&test_request());
    if is_final {
        accept(&mut reducer, TEST_STOP).unwrap();
    }
    usage_snapshot(&mut reducer, first).unwrap();
    let outcome = usage_snapshot(&mut reducer, next);
    (reducer, outcome)
}

#[test]
fn chat_completions_partial_usage_rejects_conflicts_against_merged_observations() {
    use ProtocolError::{ConflictingIdentity, InvalidChunk};
    let cases = [
        (
            r#"{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}"#,
            r#"{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}"#,
            true,
            ConflictingIdentity,
        ),
        (
            r#"{"prompt_tokens":10}"#,
            r#"{"prompt_tokens":9}"#,
            false,
            ConflictingIdentity,
        ),
        (
            r#"{"completion_tokens":3}"#,
            r#"{"completion_tokens":2}"#,
            false,
            ConflictingIdentity,
        ),
        (
            r#"{"total_tokens":13}"#,
            r#"{"total_tokens":12}"#,
            false,
            ConflictingIdentity,
        ),
        (
            r#"{"prompt_tokens":10}"#,
            r#"{"prompt_tokens":11}"#,
            true,
            ConflictingIdentity,
        ),
        (
            r#"{"completion_tokens":3}"#,
            r#"{"completion_tokens":4}"#,
            true,
            ConflictingIdentity,
        ),
        (
            r#"{"total_tokens":13}"#,
            r#"{"total_tokens":14}"#,
            true,
            ConflictingIdentity,
        ),
        (
            r#"{"prompt_tokens":10}"#,
            r#"{"completion_tokens":-1}"#,
            false,
            InvalidChunk,
        ),
        (
            r#"{"prompt_tokens":10}"#,
            r#"{"total_tokens":-1}"#,
            true,
            InvalidChunk,
        ),
        (
            r#"{"prompt_tokens":10}"#,
            r#"{"completion_tokens":1.5}"#,
            true,
            InvalidChunk,
        ),
        (
            r#"{"prompt_tokens":10}"#,
            r#"{"total_tokens":"13"}"#,
            false,
            InvalidChunk,
        ),
    ];
    for (first, next, is_final, failure) in cases {
        let mut probe = new_reducer(&test_request());
        if is_final {
            accept(&mut probe, TEST_STOP).unwrap();
        }
        usage_snapshot(&mut probe, first).unwrap();
        let previous = (probe.usage, probe.final_fields);
        let (reducer, outcome) = usage_follow_up(first, next, is_final);
        assert_eq!(outcome, Err(failure), "{first} then {next}");
        assert_eq!((reducer.usage, reducer.final_fields), previous);
        assert_eq!(reducer.phase, Phase::Closed);
    }
}

#[test]
fn chat_completions_partial_usage_accepts_totals_that_disagree_with_their_parts() {
    let cases = [
        (
            r#"{"prompt_tokens":10}"#,
            r#"{"completion_tokens":3,"total_tokens":12}"#,
            false,
        ),
        (
            r#"{"total_tokens":13}"#,
            r#"{"prompt_tokens":10,"completion_tokens":2}"#,
            false,
        ),
        (
            r#"{"prompt_tokens":10}"#,
            r#"{"prompt_tokens":10,"completion_tokens":3,"total_tokens":14}"#,
            false,
        ),
        (
            r#"{"prompt_tokens":10}"#,
            r#"{"completion_tokens":3,"total_tokens":14}"#,
            true,
        ),
        (
            r#"{"prompt_tokens":10,"total_tokens":13}"#,
            r#"{"completion_tokens":2}"#,
            true,
        ),
        (
            r#"{"prompt_tokens":9223372036854775807}"#,
            r#"{"completion_tokens":9223372036854775807,"total_tokens":9223372036854775807}"#,
            false,
        ),
        (
            r#"{"prompt_tokens":10,"total_tokens":13}"#,
            r#"{"completion_tokens":4}"#,
            true,
        ),
    ];
    for (first, next, is_final) in cases {
        let (_, outcome) = usage_follow_up(first, next, is_final);
        assert_eq!(outcome, Ok(()), "{first} then {next}");
    }
}

#[test]
fn chat_completions_opaque_number_preservation_does_not_relax_index_or_usage_validation() {
    for number in ["-1", "0.0", "1e0", "9223372036854775808", "\"0\"", "null"] {
        let mut reducer = new_reducer(&test_request());
        let chunk = format!(r#"{{"choices":[{{"index":{number},"delta":{{}}}}]}}"#);
        assert_eq!(
            accept(&mut reducer, &chunk),
            Err(ProtocolError::InvalidChunk),
            "{number}"
        );
    }
    for number in ["-1", "0.0", "1e0", "9223372036854775808", "\"0\""] {
        let mut reducer = new_reducer(&test_request());
        accept(&mut reducer, TEST_STOP).unwrap();
        let chunk = format!(r#"{{"choices":[],"usage":{{"completion_tokens":{number}}}}}"#);
        assert_eq!(
            accept(&mut reducer, &chunk),
            Err(ProtocolError::InvalidChunk),
            "{number}"
        );
    }
}

#[test]
fn chat_completions_omits_tool_controls_when_no_tools_are_advertised() {
    let body = build(&test_request(), ToolChoiceMode::Send).unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert!(parsed.get("tools").is_none());
    assert!(parsed.get("tool_choice").is_none());
    assert!(parsed.get("parallel_tool_calls").is_none());
}

#[test]
fn chat_completions_exact_text_wire_preserves_instruction_order_and_opaque_model() {
    let body = build(&test_request(), ToolChoiceMode::Omit).unwrap();
    assert_eq!(
        body,
        r#"{"model":"opaque/local-model:8b","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"system","content":"first"},{"role":"system","content":"second"},{"role":"user","content":"hi"}]}"#
    );
    assert_eq!(build(&test_request(), ToolChoiceMode::Omit).unwrap(), body);
}

#[test]
fn chat_completions_exact_tool_wire_orders_tools_choice_and_each_token_limit() {
    let mut request = test_tool_request();
    request.max_output_tokens = Some(77);
    let cases = [
        (
            MaxTokensParameter::MaxTokens,
            r#"{"model":"opaque/local-model:8b","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"system","content":"first"},{"role":"system","content":"second"},{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"read_file","description":"Read a file.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false,"required":["path"]}}},{"type":"function","function":{"name":"shell","description":"Run a command.","parameters":{"type":"object","properties":{"request":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}}}],"tool_choice":"auto","max_tokens":77}"#,
        ),
        (
            MaxTokensParameter::MaxCompletionTokens,
            r#"{"model":"opaque/local-model:8b","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"system","content":"first"},{"role":"system","content":"second"},{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"read_file","description":"Read a file.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false,"required":["path"]}}},{"type":"function","function":{"name":"shell","description":"Run a command.","parameters":{"type":"object","properties":{"request":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}}}],"tool_choice":"auto","max_completion_tokens":77}"#,
        ),
    ];
    for (max_tokens_parameter, expected) in cases {
        let options = RequestOptions {
            tool_choice_mode: ToolChoiceMode::Send,
            max_tokens_parameter,
        };
        let body = build_request(&request.borrowed(), options).unwrap().body;
        assert_eq!(String::from_utf8(body).unwrap(), expected);
    }
}

#[test]
fn chat_completions_tool_wire_carries_nested_schemas() {
    let mut request = test_tool_request();
    request.tools.push(tool(
        "mcp_docs",
        "Find docs.",
        r#"{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}"#,
    ));
    let body = build(&request, ToolChoiceMode::Omit).unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    let tools = parsed["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    for tool in tools {
        assert_eq!(tool.as_object().unwrap().len(), 2);
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"].as_object().unwrap().len(), 3);
        assert!(tool["function"]["parameters"].is_object());
    }
    assert_eq!(
        tools[1]["function"]["parameters"]["properties"]["request"]["required"][0],
        "command"
    );
    let mut reducer = new_reducer(&request);
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"mcp-1","function":{"name":"mcp_docs","arguments":"{\"query\":\"zig\"}"}}]}}]}"#,
    )
    .unwrap();
    let completion = finish_with(&mut reducer, TEST_TOOLS_FINISH).unwrap();
    assert_eq!(completion.tool_calls[0].name, "mcp_docs");
}

#[test]
fn chat_completions_history_correlation_preserves_canonical_ids_and_json_strings() {
    let call = call("functions/read:0", "read_file", r#"{"path":"a\"b"}"#);
    let mut request = test_tool_request();
    request.messages = vec![
        ChatMessage::Assistant {
            content: Some("reading".to_owned()),
            tool_calls: vec![call.clone()],
            provider_replay: None,
        },
        tool_result("functions/read:0", "read_file", "result"),
        ChatMessage::user("continue"),
    ];
    let body = build(&request, ToolChoiceMode::Omit).unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    let messages = parsed["messages"].as_array().unwrap();
    let wire_call = &messages[2]["tool_calls"][0];
    let wire_id = wire_call["id"].as_str().unwrap();
    assert!(wire_id.starts_with("fx_"));
    assert_eq!(messages[3]["tool_call_id"], wire_id);
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(wire_call["function"]["arguments"], call.arguments);
    assert_eq!(call.id.as_str(), "functions/read:0");
}

#[test]
fn chat_completions_requests_ignore_provider_replay_state() {
    let mut request = test_request();
    let answer = |provider_replay| ChatMessage::Assistant {
        content: Some("answer".to_owned()),
        tool_calls: Vec::new(),
        provider_replay,
    };
    request.messages = vec![
        ChatMessage::user("question"),
        answer(None),
        ChatMessage::user("continue"),
    ];
    let plain = build(&request, ToolChoiceMode::Omit).unwrap();
    request.messages[1] = answer(Some(ProviderReplay {
        source: ReplaySource {
            provider: "codex".to_owned(),
            model: "model".to_owned(),
        },
        parts_json: r#"[{"type":"reasoning","encrypted_content":"cipher"}]"#.to_owned(),
    }));
    assert_eq!(build(&request, ToolChoiceMode::Omit).unwrap(), plain);
}

#[test]
fn chat_completions_tool_choice_controls_and_deliberate_basic_option_mapping() {
    for choice in [ToolChoice::Auto, ToolChoice::None, ToolChoice::Required] {
        for mode in [ToolChoiceMode::Omit, ToolChoiceMode::Send] {
            let mut request = test_tool_request();
            request.tool_choice = choice;
            request.max_output_tokens = Some(73);
            let body = build(&request, mode).unwrap();
            let fields: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(fields.get("tools").is_some(), choice != ToolChoice::None);
            let sends_choice = mode == ToolChoiceMode::Send && choice != ToolChoice::None;
            assert_eq!(fields.get("tool_choice").is_some(), sends_choice);
            if sends_choice {
                assert_eq!(fields["tool_choice"], choice.as_str());
            }
            assert_eq!(fields["max_tokens"], 73);
            assert!(fields.get("max_completion_tokens").is_none());
            assert!(fields.get("providerOptions").is_none());
        }
    }
    let mut request = test_request();
    request.tool_choice = ToolChoice::Required;
    assert_eq!(
        build(&request, ToolChoiceMode::Omit),
        Err(ProtocolError::RequiredToolMissing)
    );
    let mut request = test_tool_request();
    request.tool_choice = ToolChoice::None;
    let mut none = new_reducer(&request);
    assert_eq!(
        accept(&mut none, TEST_CALL),
        Err(ProtocolError::UnexpectedToolCall)
    );
    request.tool_choice = ToolChoice::Required;
    let mut required = new_reducer(&request);
    accept(&mut required, TEST_TEXT).unwrap();
    assert_eq!(
        finish_with(&mut required, TEST_STOP),
        Err(ProtocolError::RequiredToolMissing)
    );
}

#[test]
fn chat_completions_rejects_unsupported_requests_and_ambiguous_selection() {
    let mut request = test_request();
    request.max_output_tokens = Some(0);
    assert_eq!(
        build(&request, ToolChoiceMode::Omit),
        Err(ProtocolError::InvalidOutputLimit)
    );
    let mut request = test_request();
    request.model = String::new();
    assert_eq!(
        build(&request, ToolChoiceMode::Omit),
        Err(ProtocolError::InvalidModel)
    );
    let mut request = test_request();
    request.messages = vec![ChatMessage::System {
        content: "untrusted".to_owned(),
    }];
    assert_eq!(
        build(&request, ToolChoiceMode::Omit),
        Err(ProtocolError::InvalidProviderPrompt)
    );
    let mut request = test_tool_request();
    request.tools.push(test_tools().remove(0));
    assert_eq!(
        build(&request, ToolChoiceMode::Omit),
        Err(ProtocolError::InvalidToolSelection)
    );
    let mut request = test_request();
    request.tools = vec![tool("bad name", "", "{}")];
    assert_eq!(
        build(&request, ToolChoiceMode::Omit),
        Err(ProtocolError::InvalidToolName)
    );
}

#[test]
fn chat_completions_rejects_unmatched_malformed_and_duplicate_history_calls() {
    let history = |calls: Vec<ToolCall>, result_id: &str| {
        let mut request = test_request();
        request.messages = vec![
            ChatMessage::Assistant {
                content: None,
                tool_calls: calls,
                provider_replay: None,
            },
            tool_result(result_id, "read_file", "result"),
        ];
        build(&request, ToolChoiceMode::Omit)
    };
    assert_eq!(
        history(vec![call("call-1", "read_file", "{}")], "other"),
        Err(ProtocolError::InvalidToolHistory)
    );
    assert_eq!(
        history(vec![call("call-1", "read_file", "[]")], "call-1"),
        Err(ProtocolError::InvalidToolArguments)
    );
    let duplicate = call("call-1", "read_file", "{}");
    assert_eq!(
        history(vec![duplicate.clone(), duplicate], "call-1"),
        Err(ProtocolError::InvalidToolHistory)
    );
    assert!(history(vec![call("call-1", "read_file", "{}")], "call-1").is_ok());
}

#[test]
fn chat_completions_accepts_matching_empty_terminal_usage_choices() {
    for with_tools in [false, true] {
        let mut reducer = new_reducer(&test_tool_request());
        accept(&mut reducer, if with_tools { TEST_CALL } else { TEST_TEXT }).unwrap();
        accept(
            &mut reducer,
            if with_tools {
                TEST_TOOLS_FINISH
            } else {
                TEST_STOP
            },
        )
        .unwrap();
        let trailer = format!(
            r#"{{"choices":[{{"index":0,"delta":{{"role":"assistant","content":""}},"finish_reason":"{}"}}],"usage":{{"prompt_tokens":16,"completion_tokens":6,"total_tokens":22}}}}"#,
            if with_tools { "tool_calls" } else { "stop" }
        );
        assert_eq!(
            reducer.accept(trailer.as_bytes(), false).unwrap().content,
            None
        );
        accept(&mut reducer, "[DONE]").unwrap();
        let completion = reducer.finish(false).unwrap();
        assert_eq!(completion.usage.input_tokens, Some(16));
        assert_eq!(completion.usage.output_tokens, Some(6));
        assert_eq!(completion.tool_calls.len(), usize::from(with_tools));
    }
}

#[test]
fn chat_completions_terminal_usage_cannot_introduce_content_tools_or_a_different_finish() {
    let cases = [
        (r#"{"content":"late"}"#, "stop"),
        (r#"{"tool_calls":[{}]}"#, "stop"),
        (r#"{"refusal":"blocked"}"#, "stop"),
        (r#"{"reasoning":"late"}"#, "stop"),
        (r#"{"role":"user"}"#, "stop"),
        ("{}", "tool_calls"),
    ];
    for (delta, reason) in cases {
        let mut reducer = new_reducer(&test_tool_request());
        accept(&mut reducer, TEST_STOP).unwrap();
        let trailer = format!(
            r#"{{"choices":[{{"index":0,"delta":{delta},"finish_reason":"{reason}"}}],"usage":{{"prompt_tokens":16,"completion_tokens":6,"total_tokens":22}}}}"#
        );
        assert_eq!(
            accept(&mut reducer, &trailer),
            Err(ProtocolError::InconsistentFinishReason)
        );
        assert_eq!(reducer.finish(false), Err(ProtocolError::StreamClosed));
    }
    let mut reducer = new_reducer(&test_request());
    accept(&mut reducer, TEST_STOP).unwrap();
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    )
    .unwrap();
    assert_eq!(reducer.finish(false), Err(ProtocolError::IncompleteStream));
}

#[test]
fn chat_completions_repeated_terminal_choices_reject_invalid_or_conflicting_usage() {
    let cases = [
        (
            r#"{"prompt_tokens":-1}"#,
            false,
            ProtocolError::InvalidChunk,
        ),
        (
            r#"{"prompt_tokens":1.5}"#,
            false,
            ProtocolError::InvalidChunk,
        ),
        (
            r#"{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}"#,
            true,
            ProtocolError::ConflictingIdentity,
        ),
    ];
    for (usage, seed_usage, expected) in cases {
        let mut reducer = new_reducer(&test_request());
        accept(&mut reducer, TEST_STOP).unwrap();
        if seed_usage {
            accept(
                &mut reducer,
                r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            )
            .unwrap();
        }
        let trailer = format!(
            r#"{{"choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}],"usage":{usage}}}"#
        );
        assert_eq!(accept(&mut reducer, &trailer), Err(expected), "{usage}");
        assert_eq!(reducer.finish(false), Err(ProtocolError::StreamClosed));
    }
}

#[test]
fn chat_completions_owns_fragmented_interleaved_tools_and_results_independently() {
    let mut reducer = new_reducer(&test_tool_request());
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call-2","function":{"name":"sh","arguments":"{\"request\":"}},{"index":0,"id":"call-1","function":{"name":"read_","arguments":"{\"path\":"}}]}}]}"#,
    )
    .unwrap();
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"\"a\"}"}},{"index":1,"function":{"name":"ell","arguments":"{\"command\":\"pwd\"}}"}}]}}]}"#,
    )
    .unwrap();
    let completion = finish_with(&mut reducer, TEST_TOOLS_FINISH).unwrap();
    assert_eq!(completion.tool_calls.len(), 2);
    assert_eq!(completion.tool_calls[0].id.as_str(), "call-1");
    assert_eq!(completion.tool_calls[0].name, "read_file");
    assert_eq!(completion.tool_calls[0].arguments, r#"{"path":"a"}"#);
    assert_eq!(
        completion.tool_calls[1].arguments,
        r#"{"request":{"command":"pwd"}}"#
    );
    assert_eq!(completion.finish_reason, FinishReason::ToolCalls);
}

#[test]
fn chat_completions_malformed_and_nonobject_final_arguments_never_become_tools() {
    for arguments in ["", "{", "[]", "null", "3", "{}junk", r#"{"x":1,"x":2}"#] {
        let mut reducer = new_reducer(&test_tool_request());
        let chunk = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":arguments}}]}}]});
        accept(&mut reducer, &chunk.to_string()).unwrap();
        assert_eq!(
            finish_with(&mut reducer, TEST_TOOLS_FINISH),
            Err(ProtocolError::InvalidToolArguments),
            "{arguments}"
        );
        assert_eq!(reducer.finish(false), Err(ProtocolError::StreamClosed));
    }
}

#[test]
fn objects_the_upstream_parser_accepts_become_tools_and_replay() {
    for arguments in [
        r#"{"limit":1e999}"#,
        r#"{"offset":123456789012345678901234567890,"scale":-1.5E-999}"#,
        r#" {"nested":[1,null,{}]} "#,
    ] {
        let mut reducer = new_reducer(&test_tool_request());
        let chunk = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":arguments}}]}}]});
        accept(&mut reducer, &chunk.to_string()).unwrap();
        let completion = finish_with(&mut reducer, TEST_TOOLS_FINISH).unwrap();
        assert_eq!(completion.tool_calls[0].arguments, arguments);

        let mut request = test_request();
        request.messages = vec![
            ChatMessage::Assistant {
                content: None,
                tool_calls: completion.tool_calls,
                provider_replay: None,
            },
            tool_result("call-1", "read_file", "result"),
        ];
        assert!(build(&request, ToolChoiceMode::Omit).is_ok(), "{arguments}");
    }
}

#[test]
fn chat_completions_terminal_evidence_and_finish_reasons_are_strict() {
    let mut early_done = new_reducer(&test_request());
    assert_eq!(
        accept(&mut early_done, "[DONE]"),
        Err(ProtocolError::IncompleteStream)
    );
    let mut no_done = new_reducer(&test_request());
    accept(&mut no_done, TEST_STOP).unwrap();
    assert_eq!(no_done.finish(false), Err(ProtocolError::IncompleteStream));
    for (reason, failure) in [
        ("length", ProtocolError::OutputTruncated),
        ("content_filter", ProtocolError::ContentFiltered),
        ("error", ProtocolError::ProviderError),
    ] {
        let mut reducer = new_reducer(&test_tool_request());
        accept(&mut reducer, TEST_CALL).unwrap();
        let terminal =
            format!(r#"{{"choices":[{{"index":0,"delta":{{}},"finish_reason":"{reason}"}}]}}"#);
        assert_eq!(finish_with(&mut reducer, &terminal), Err(failure));
    }
    let mut refused = new_reducer(&test_request());
    accept(
        &mut refused,
        r#"{"choices":[{"index":0,"delta":{"refusal":"No."},"finish_reason":"stop"}]}"#,
    )
    .unwrap();
    accept(&mut refused, "[DONE]").unwrap();
    assert_eq!(refused.finish(false), Err(ProtocolError::Refused));
    let mut inconsistent = new_reducer(&test_tool_request());
    assert_eq!(
        finish_with(&mut inconsistent, TEST_TOOLS_FINISH),
        Err(ProtocolError::InconsistentFinishReason)
    );
}

#[test]
fn chat_completions_rejects_malformed_chunks_contradictory_identities_and_extra_choices() {
    use ProtocolError::{
        ConflictingIdentity, InconsistentFinishReason, InvalidChunk, InvalidToolName,
    };
    let cases: [(Option<&str>, &str, ProtocolError); 16] = [
        (None, "not JSON", InvalidChunk),
        (None, r#"{"choices":[],"choices":[]}"#, InvalidChunk),
        (
            None,
            r#"{"error":{"message":"bad request"}}"#,
            ProtocolError::ProviderError,
        ),
        (
            None,
            r#"{"choices":[{"index":1,"delta":{}}]}"#,
            InvalidChunk,
        ),
        (
            None,
            r#"{"choices":[{"index":0,"delta":{}},{"index":1,"delta":{}}]}"#,
            InvalidChunk,
        ),
        (
            None,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":7}]}"#,
            InvalidChunk,
        ),
        (Some(TEST_STOP), TEST_TOOLS_FINISH, InconsistentFinishReason),
        (
            Some(TEST_TEXT),
            r#"{"id":"other","choices":[]}"#,
            ConflictingIdentity,
        ),
        (
            Some(TEST_TEXT),
            r#"{"model":"other","choices":[]}"#,
            ConflictingIdentity,
        ),
        (
            Some(TEST_CALL),
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"other"}]}}]}"#,
            ConflictingIdentity,
        ),
        (
            Some(TEST_CALL),
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"shell"}}]}}]}"#,
            InvalidToolName,
        ),
        (
            Some(TEST_CALL),
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call-1"}]}}]}"#,
            ConflictingIdentity,
        ),
        (
            None,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":-1}]}}]}"#,
            InvalidChunk,
        ),
        (
            None,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0},{"index":0}]}}]}"#,
            ConflictingIdentity,
        ),
        (
            None,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"type":"web_search"}]}}]}"#,
            InvalidChunk,
        ),
        (
            None,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":{}}}]}"#,
            InvalidChunk,
        ),
    ];
    for (first, chunk, failure) in cases {
        let mut reducer = new_reducer(&test_tool_request());
        if let Some(first) = first {
            accept(&mut reducer, first).unwrap();
        }
        assert_eq!(accept(&mut reducer, chunk), Err(failure), "{chunk}");
        assert_eq!(
            accept(&mut reducer, TEST_STOP),
            Err(ProtocolError::StreamClosed)
        );
    }
}

#[test]
fn chat_completions_accepts_empty_choice_chunks_before_finish() {
    let mut reducer = new_reducer(&test_request());
    accept(
        &mut reducer,
        r#"{"choices":[],"created":0,"id":"","model":"","object":"","prompt_filter_results":[{"prompt_index":0,"content_filter_results":{}}]}"#,
    )
    .unwrap();
    accept(&mut reducer, TEST_TEXT).unwrap();
    accept(
        &mut reducer,
        r#"{"choices":[],"usage":{"prompt_tokens":3}}"#,
    )
    .unwrap();
    let completion = finish_with(&mut reducer, TEST_STOP).unwrap();
    assert_eq!(completion.content.as_deref(), Some("hello"));
    assert_eq!(completion.usage.input_tokens, Some(3));
}

#[test]
fn chat_completions_usage_trailers_preserve_observations() {
    let mut reducer = new_reducer(&test_request());
    accept(&mut reducer, TEST_TEXT).unwrap();
    accept(&mut reducer, TEST_STOP).unwrap();
    let usage =
        r#"{"choices":[],"usage":{"prompt_tokens":17,"completion_tokens":3,"total_tokens":20}}"#;
    accept(&mut reducer, usage).unwrap();
    accept(&mut reducer, usage).unwrap();
    accept(&mut reducer, "[DONE]").unwrap();
    let completion = reducer.finish(false).unwrap();
    assert_eq!(completion.content.as_deref(), Some("hello"));
    assert_eq!(completion.usage.input_tokens, Some(17));
    assert_eq!(completion.usage.output_tokens, Some(3));
    assert_eq!(reducer.finish(false), Err(ProtocolError::StreamClosed));
    for invalid in [
        r#"{"choices":[],"usage":{"prompt_tokens":-1}}"#,
        r#"{"choices":[],"usage":{"completion_tokens":1.5}}"#,
    ] {
        let mut bad = new_reducer(&test_request());
        accept(&mut bad, TEST_STOP).unwrap();
        assert_eq!(accept(&mut bad, invalid), Err(ProtocolError::InvalidChunk));
    }
}

#[test]
fn chat_completions_reducer_caps_events_content_identities_arguments_tools_and_nesting() {
    let nested = format!("{}{}", "[".repeat(65), "]".repeat(65));
    let cases = [
        (
            Limits {
                event_bytes: 1,
                ..Limits::default()
            },
            TEST_TEXT,
            ProtocolError::EventTooLarge,
        ),
        (
            Limits {
                total_wire_bytes: 1,
                ..Limits::default()
            },
            TEST_TEXT,
            ProtocolError::StreamTooLarge,
        ),
        (
            Limits {
                events: 0,
                ..Limits::default()
            },
            TEST_TEXT,
            ProtocolError::TooManyEvents,
        ),
        (
            Limits {
                identity_bytes: 2,
                ..Limits::default()
            },
            TEST_TEXT,
            ProtocolError::IdentityTooLarge,
        ),
        (
            Limits {
                content_bytes: 4,
                ..Limits::default()
            },
            TEST_TEXT,
            ProtocolError::ContentTooLarge,
        ),
        (
            Limits {
                arguments_bytes: 2,
                ..Limits::default()
            },
            TEST_CALL,
            ProtocolError::ArgumentsTooLarge,
        ),
        (
            Limits {
                tool_calls: 0,
                ..Limits::default()
            },
            TEST_CALL,
            ProtocolError::TooManyTools,
        ),
        (
            Limits::default(),
            nested.as_str(),
            ProtocolError::JsonTooDeep,
        ),
    ];
    for (limits, chunk, failure) in cases {
        let mut reducer = Reducer::new(selection(&test_tool_request()), limits);
        assert_eq!(accept(&mut reducer, chunk), Err(failure));
    }
    let limits = Limits {
        content_bytes: 5,
        events: 3,
        ..Limits::default()
    };
    let mut reducer = Reducer::new(selection(&test_request()), limits);
    accept(&mut reducer, TEST_TEXT).unwrap();
    assert!(finish_with(&mut reducer, TEST_STOP).is_ok());
}

#[test]
fn chat_completions_cancellation_poisons_retained_state() {
    let mut reducer = new_reducer(&test_tool_request());
    accept(&mut reducer, TEST_CALL).unwrap();
    assert_eq!(
        reducer.accept(TEST_TOOLS_FINISH.as_bytes(), true),
        Err(ProtocolError::Cancelled)
    );
    assert_eq!(reducer.finish(false), Err(ProtocolError::StreamClosed));
}

#[test]
fn chat_completions_name_fragments_preserve_repeated_bytes_and_shared_prefixes() {
    let mut request = test_request();
    request.tools = vec![
        tool("read", "Read.", "{}"),
        tool("read_file", "Read file.", "{}"),
        tool("aa", "Repeated bytes.", "{}"),
    ];
    let mut reducer = new_reducer(&request);
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"one","function":{"name":"read","arguments":"{}"}},{"index":1,"id":"two","function":{"name":"a","arguments":"{}"}}]}}]}"#,
    )
    .unwrap();
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"_file"}},{"index":1,"function":{"name":"a"}}]}}]}"#,
    )
    .unwrap();
    let completion = finish_with(&mut reducer, TEST_TOOLS_FINISH).unwrap();
    assert_eq!(completion.tool_calls[0].name, "read_file");
    assert_eq!(completion.tool_calls[1].name, "aa");
}

#[test]
fn chat_completions_accepts_empty_name_fragments_without_losing_arguments() {
    let mut reducer = new_reducer(&test_tool_request());
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{"}}]}}]}"#,
    )
    .unwrap();
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"","arguments":"\"path\":\"x\"}"}}]}}]}"#,
    )
    .unwrap();
    let completion = finish_with(&mut reducer, TEST_TOOLS_FINISH).unwrap();
    assert_eq!(completion.tool_calls[0].name, "read_file");
    assert_eq!(completion.tool_calls[0].arguments, r#"{"path":"x"}"#);
}

#[test]
fn chat_completions_accepts_echoed_models_up_to_the_request_model_limit() {
    let mut request = test_request();
    request.model = "m".repeat(MAX_MODEL_BYTES);
    assert!(build(&request, ToolChoiceMode::Omit).is_ok());
    let mut reducer = new_reducer(&request);
    let chunk = format!(
        r#"{{"model":"{}","choices":[{{"index":0,"delta":{{"content":"ok"}},"finish_reason":"stop"}}]}}"#,
        request.model
    );
    accept(&mut reducer, &chunk).unwrap();
    accept(&mut reducer, "[DONE]").unwrap();
    assert_eq!(
        reducer.finish(false).unwrap().content.as_deref(),
        Some("ok")
    );
}

#[test]
fn chat_completions_model_bounds_are_enforced_before_serialization() {
    let too_long = "m".repeat(MAX_MODEL_BYTES + 1);
    for invalid in [" leading", "trailing ", "bad\u{7f}", too_long.as_str()] {
        let mut request = test_request();
        request.model = invalid.to_owned();
        assert_eq!(
            build(&request, ToolChoiceMode::Omit),
            Err(ProtocolError::InvalidModel)
        );
    }
}

#[test]
fn chat_completions_missing_tool_fields_sparse_indexes_and_invalid_identities_fail_closed() {
    use ProtocolError::{
        InvalidChunk, InvalidToolArguments, InvalidToolCallId, InvalidToolName, TooManyTools,
    };
    let cases = [
        (
            r#"{"index":0,"function":{"name":"read_file","arguments":"{}"}}"#,
            InvalidToolCallId,
            true,
        ),
        (
            r#"{"index":0,"id":"x","function":{"arguments":"{}"}}"#,
            InvalidToolName,
            true,
        ),
        (
            r#"{"index":0,"id":"x","function":{"name":"read_file"}}"#,
            InvalidToolArguments,
            true,
        ),
        (
            r#"{"index":1,"id":"x","function":{"name":"read_file","arguments":"{}"}}"#,
            InvalidToolCallId,
            true,
        ),
        (r#"{"index":0,"id":""}"#, InvalidToolCallId, true),
        (r#"{"index":0,"id":7}"#, InvalidChunk, false),
        (
            r#"{"index":0,"function":{"name":"not_advertised"}}"#,
            InvalidToolName,
            false,
        ),
        (r#"{"index":0,"function":{"name":3}}"#, InvalidChunk, false),
        (
            r#"{"index":0,"function":{"arguments":{}}}"#,
            InvalidChunk,
            false,
        ),
        (r#"{"index":999999999}"#, TooManyTools, false),
    ];
    for (delta, failure, at_finish) in cases {
        let mut reducer = new_reducer(&test_tool_request());
        let chunk = format!(r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{delta}]}}}}]}}"#);
        if at_finish {
            accept(&mut reducer, &chunk).unwrap();
            assert_eq!(
                finish_with(&mut reducer, TEST_TOOLS_FINISH),
                Err(failure),
                "{delta}"
            );
        } else {
            assert_eq!(accept(&mut reducer, &chunk), Err(failure), "{delta}");
        }
    }
}

#[test]
fn chat_completions_repeated_consistent_identity_preserves_one_call() {
    let mut request = test_request();
    request.tools = vec![tool("read_file", "Read.", "{}")];
    let mut reducer = new_reducer(&request);
    accept(&mut reducer, TEST_CALL).unwrap();
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"arguments":""}}]}}]}"#,
    )
    .unwrap();
    let completion = finish_with(&mut reducer, TEST_TOOLS_FINISH).unwrap();
    assert_eq!(completion.tool_calls.len(), 1);
    assert_eq!(completion.tool_calls[0].arguments, r#"{"path":"x"}"#);
}

#[test]
fn chat_completions_usage_conflicts_late_errors_and_post_terminal_data_cannot_succeed() {
    let mut conflict = new_reducer(&test_request());
    accept(&mut conflict, TEST_STOP).unwrap();
    accept(
        &mut conflict,
        r#"{"choices":[],"usage":{"prompt_tokens":1}}"#,
    )
    .unwrap();
    assert_eq!(
        accept(
            &mut conflict,
            r#"{"choices":[],"usage":{"prompt_tokens":2}}"#
        ),
        Err(ProtocolError::ConflictingIdentity)
    );
    let mut late_error = new_reducer(&test_request());
    accept(&mut late_error, TEST_STOP).unwrap();
    assert_eq!(
        accept(&mut late_error, r#"{"error":{"message":"late error"}}"#),
        Err(ProtocolError::ProviderError)
    );
    assert_eq!(
        late_error.take_failure_detail().as_deref(),
        Some(r#"{"message":"late error"}"#)
    );
    let mut late_text = new_reducer(&test_request());
    accept(&mut late_text, TEST_STOP).unwrap();
    assert_eq!(
        accept(&mut late_text, TEST_TEXT),
        Err(ProtocolError::InconsistentFinishReason)
    );
    let mut after_done = new_reducer(&test_request());
    accept(&mut after_done, TEST_STOP).unwrap();
    accept(&mut after_done, "[DONE]").unwrap();
    assert_eq!(
        accept(&mut after_done, "[DONE]"),
        Err(ProtocolError::StreamClosed)
    );
    assert_eq!(after_done.finish(false), Err(ProtocolError::StreamClosed));
}

#[test]
fn chat_completions_aggregate_delta_bounds_include_every_event() {
    let cases = [
        (
            Limits {
                total_wire_bytes: TEST_TEXT.len(),
                ..Limits::default()
            },
            ProtocolError::StreamTooLarge,
        ),
        (
            Limits {
                events: 1,
                ..Limits::default()
            },
            ProtocolError::TooManyEvents,
        ),
        (
            Limits {
                content_bytes: 9,
                ..Limits::default()
            },
            ProtocolError::ContentTooLarge,
        ),
    ];
    for (limits, failure) in cases {
        let mut reducer = Reducer::new(selection(&test_request()), limits);
        accept(&mut reducer, TEST_TEXT).unwrap();
        assert_eq!(accept(&mut reducer, TEST_TEXT), Err(failure));
    }
    let limits = Limits {
        arguments_bytes: 12,
        ..Limits::default()
    };
    let mut reducer = Reducer::new(selection(&test_tool_request()), limits);
    accept(&mut reducer, TEST_CALL).unwrap();
    assert_eq!(
        accept(
            &mut reducer,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":" "}}]}}]}"#
        ),
        Err(ProtocolError::ArgumentsTooLarge)
    );
}

#[test]
fn chat_completions_prose_resembling_a_call_stays_prose_and_cancellation_after_done_wins() {
    let mut reducer = new_reducer(&test_tool_request());
    accept(
        &mut reducer,
        r#"{"choices":[{"index":0,"delta":{"content":"{\"name\":\"shell\",\"arguments\":{}}"}}]}"#,
    )
    .unwrap();
    let completion = finish_with(&mut reducer, TEST_STOP).unwrap();
    assert!(completion.tool_calls.is_empty());
    assert_eq!(
        completion.content.as_deref(),
        Some(r#"{"name":"shell","arguments":{}}"#)
    );
    let mut cancelled = new_reducer(&test_request());
    accept(&mut cancelled, TEST_STOP).unwrap();
    accept(&mut cancelled, "[DONE]").unwrap();
    assert_eq!(cancelled.finish(true), Err(ProtocolError::Cancelled));
}

#[test]
fn tool_descriptions_are_capped_at_a_character_boundary() {
    let long = "é".repeat(600);
    let capped = capped_description(&long);
    assert!(capped.len() <= DESCRIPTION_MAX_BYTES);
    assert!(capped.ends_with(TRUNCATION_MARKER));
    assert_eq!(capped_description("short"), "short");
}

fn outcome(request: &OwnedRequest, events: &[&str]) -> ProtocolResult<Completion> {
    let mut reducer = new_reducer(request);
    for event in events {
        accept(&mut reducer, event)?;
    }
    if !reducer.is_done() {
        reducer.end_of_stream()?;
    }
    reducer.finish(false)
}

#[test]
fn chat_completions_deliberately_tolerates_common_gateway_dialects() {
    let text = r#"{"choices":[{"index":0,"delta":{"content":"x"}}]}"#;
    let request = test_tool_request();
    let call = |id: &str, extra: &str| {
        format!(
            r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{{extra}"id":"{id}","type":"function","function":{{"name":"read_file","arguments":"{{}}"}}}}]}}}}]}}"#
        )
    };
    let stop_tools = [
        call("call-1", r#""index":0,"#),
        TEST_STOP.to_owned(),
        "[DONE]".to_owned(),
    ];
    let without_index = [
        call("call-1", ""),
        call("call-2", ""),
        TEST_TOOLS_FINISH.to_owned(),
    ];
    let cases: [(&str, Vec<String>, FinishReason, usize); 8] = [
        (
            "provider-native finish reason",
            vec![text.to_owned(), r#"{"choices":[{"index":0,"delta":{},"finish_reason":"end_turn"}]}"#.to_owned(), "[DONE]".to_owned()],
            FinishReason::Stop,
            0,
        ),
        (
            "provider-native finish after tool calls",
            vec![call("call-1", r#""index":0,"#), r#"{"choices":[{"index":0,"delta":{},"finish_reason":"STOP"}]}"#.to_owned()],
            FinishReason::ToolCalls,
            1,
        ),
        ("tool calls finished with stop", stop_tools.to_vec(), FinishReason::ToolCalls, 1),
        ("tool calls without an index", without_index.to_vec(), FinishReason::ToolCalls, 2),
        (
            "empty id on a continuation delta",
            vec![
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#.to_owned(),
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"","function":{"arguments":"{}"}}]}}]}"#.to_owned(),
                TEST_TOOLS_FINISH.to_owned(),
            ],
            FinishReason::ToolCalls,
            1,
        ),
        (
            "function name repeated in every delta",
            vec![
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{"}}]}}]}"#.to_owned(),
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"read_file","arguments":"}"}}]}}]}"#.to_owned(),
                TEST_TOOLS_FINISH.to_owned(),
            ],
            FinishReason::ToolCalls,
            1,
        ),
        (
            "finish chunk without a delta",
            vec![text.to_owned(), r#"{"choices":[{"index":0,"finish_reason":"stop"}]}"#.to_owned()],
            FinishReason::Stop,
            0,
        ),
        (
            "repeated finish chunk without usage",
            vec![text.to_owned(), TEST_STOP.to_owned(), TEST_STOP.to_owned(), "[DONE]".to_owned()],
            FinishReason::Stop,
            0,
        ),
    ];
    for (label, events, finish_reason, calls) in cases {
        let events: Vec<&str> = events.iter().map(String::as_str).collect();
        let completion =
            outcome(&request, &events).unwrap_or_else(|error| panic!("{label}: {error:?}"));
        assert_eq!(completion.finish_reason, finish_reason, "{label}");
        assert_eq!(completion.tool_calls.len(), calls, "{label}");
    }
    let mut reducer = new_reducer(&request);
    assert_eq!(
        accept(
            &mut reducer,
            r#"{"object":"error","message":"context too long","type":"BadRequestError","code":400}"#
        ),
        Err(ProtocolError::ProviderError)
    );
    assert!(
        reducer
            .take_failure_detail()
            .unwrap()
            .contains("context too long")
    );
}

#[test]
fn chat_completions_rejects_an_echoed_name_that_could_also_extend_to_another_tool() {
    let mut request = test_request();
    request.tools = vec![tool("a", "A.", "{}"), tool("aa", "AA.", "{}")];
    let outcome = outcome(
        &request,
        &[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"one","function":{"name":"a","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"a"}}]}}]}"#,
            TEST_TOOLS_FINISH,
        ],
    );
    assert_eq!(outcome.unwrap_err(), ProtocolError::InvalidToolName);
}

#[test]
fn chat_completions_extends_an_echoed_fragment_that_is_not_an_advertised_name() {
    let mut request = test_request();
    request.tools = vec![tool("abab", "ABAB.", "{}")];
    let completion = outcome(
        &request,
        &[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"one","function":{"name":"ab","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"ab"}}]}}]}"#,
            TEST_TOOLS_FINISH,
        ],
    )
    .unwrap();
    assert_eq!(completion.tool_calls[0].name, "abab");
}

#[test]
fn reasoning_byte_accounting_matches_serialized_json_lengths() {
    for byte in 0_u8..=0x7f {
        let text = format!("a{}bé", char::from(byte));
        assert_eq!(
            encoded_json_string_len(&text),
            Value::String(text.clone()).to_string().len(),
            "{byte:#x}"
        );
    }
    let item = json!({"type":"reasoning.text","text":"line\n\"quoted\"","signature":null});
    let text = item.to_string();
    let parsed = parse_strict_json(text.as_bytes(), DuplicateKeys::BeforeValue).unwrap();
    assert_eq!(encoded_json_len(&parsed), text.len());
}
