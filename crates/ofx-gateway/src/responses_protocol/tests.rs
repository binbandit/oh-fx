use ofx_contract::ToolResultStatus;
use serde_json::json;

use super::*;

const LIMITS: StreamLimits = StreamLimits {
    aggregate_bytes: 64 * 1024,
    events: 100,
    tool_calls: 4,
    tool_identity_bytes: 1024,
    tool_arguments_bytes: 4096,
    provider_state_bytes: 4096,
};
const REPLAY_LIMITS: ReplayLimits = ReplayLimits {
    tool_calls: 128,
    tool_identity_bytes: 256,
    tool_arguments_bytes: 4096,
    provider_state_bytes: 4096,
};
const START: &str = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":""}}"#;
const FINALIZED: &str = r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_1","name":"write_file","arguments":"{\"path\":\"preview.txt\"}"}"#;
const TERMINAL: &str = r#"{"type":"response.completed","response":{"status":"completed"}}"#;

struct Stream {
    reducer: Reducer,
    emitted: String,
}

impl Stream {
    fn new() -> Self {
        Self::with_limits(LIMITS)
    }

    fn with_limits(limits: StreamLimits) -> Self {
        Self {
            reducer: Reducer::new(limits),
            emitted: String::new(),
        }
    }

    fn apply(&mut self, event: &str) -> Result<()> {
        let (result, deltas) = self.reducer.apply(event.as_bytes(), false);
        for delta in deltas {
            if let Delta::Text(text) = delta {
                self.emitted.push_str(&text);
            }
        }
        result.map(|_| ()).map_err(|rejection| rejection.error)
    }

    fn delta(&mut self, item: i64, part: i64, text: &str) -> Result<()> {
        self.apply(
            &json!({"type": "response.output_text.delta", "output_index": item, "content_index": part, "delta": text})
                .to_string(),
        )
    }

    fn final_text(&mut self, item: i64, part: i64, text: &str) -> Result<()> {
        self.apply(
            &json!({"type": "response.output_text.done", "output_index": item, "content_index": part, "text": text})
                .to_string(),
        )
    }

    fn finish(self) -> Result<ResponsesCompletion> {
        self.reducer.finish(false)
    }

    fn finish_text(mut self, expected: &str) {
        self.apply(TERMINAL).unwrap();
        assert_eq!(self.emitted, expected);
        let completion = self.finish().unwrap();
        assert_eq!(completion.content.as_deref().unwrap_or_default(), expected);
    }
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn assistant(content: Option<&str>, tool_calls: Vec<ToolCall>) -> ChatMessage {
    ChatMessage::Assistant {
        content: content.map(str::to_owned),
        tool_calls,
        provider_replay: None,
    }
}

fn tool_result(id: &str, content: &str) -> ChatMessage {
    ChatMessage::Tool {
        call_id: ToolCallId::new(id),
        tool_name: "read_file".to_owned(),
        content: content.to_owned(),
        status: ToolResultStatus::Success,
    }
}

fn input(messages: &[ChatMessage], replays: &[Option<&str>]) -> Result<Vec<Value>> {
    let mut out = String::from("[");
    write_input(&mut out, messages, replays, REPLAY_LIMITS)?;
    out.push(']');
    match serde_json::from_str(&out) {
        Ok(Value::Array(items)) => Ok(items),
        _ => panic!("input is a JSON array: {out}"),
    }
}

fn replay_of(completion: &ResponsesCompletion) -> Option<&str> {
    completion.provider_state.as_deref()
}

#[test]
fn responses_request_projects_long_call_ids_with_matching_outputs() {
    let source_id = "c".repeat(65);
    let messages = [
        assistant(None, vec![call(&source_id, "read_file", "{}")]),
        tool_result(&source_id, "result"),
    ];
    let items = input(&messages, &[None, None]).unwrap();
    let call_id = items[0]["call_id"].as_str().unwrap();
    assert!(call_id.len() <= 64);
    assert_eq!(items[1]["call_id"], call_id);
    assert_eq!(items[1]["output"], "result");
}

#[test]
fn responses_request_preserves_opaque_tool_call_identity() {
    let state = r#"[{"type":"reasoning","id":"rs_1","encrypted_content":"opaque","summary":[]}]"#;
    let messages = [
        assistant(None, vec![call("signed:0", "read_file", "{}")]),
        tool_result("signed:0", "result"),
    ];
    let items = input(&messages, &[Some(state), None]).unwrap();
    assert_eq!(items[0]["encrypted_content"], "opaque");
    assert_eq!(
        items[0].to_string(),
        r#"{"type":"reasoning","id":"rs_1","encrypted_content":"opaque","summary":[]}"#
    );
    assert_eq!(items[1]["call_id"], "signed:0");
    assert_eq!(items[2]["call_id"], "signed:0");
}

#[test]
fn responses_replay_retains_phase_through_storage_and_projection() {
    let parts = r#"[{"type":"reasoning","encrypted_content":"cipher"},{"type":"message","phase":"commentary"}]"#;
    let items = input(&[assistant(Some("original"), Vec::new())], &[Some(parts)]).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["encrypted_content"], "cipher");
    assert_eq!(items[1]["phase"], "commentary");
    assert_eq!(items[1]["content"][0]["text"], "original");
    assert_eq!(
        select_replay_parts(parts, 4096, true, false)
            .unwrap()
            .as_deref(),
        Some(r#"[{"type":"message","phase":"commentary"}]"#)
    );
    assert_eq!(
        select_replay_parts(parts, 4096, false, true)
            .unwrap()
            .as_deref(),
        Some(r#"[{"type":"reasoning","encrypted_content":"cipher"}]"#)
    );
}

#[test]
fn responses_unchanged_replay_projection_keeps_the_stored_parts() {
    let parts = r#"[ {"type":"reasoning","encrypted_content":"kept"} ]"#;
    assert_eq!(
        select_replay_parts(parts, 4096, false, true)
            .unwrap()
            .as_deref(),
        Some(parts)
    );
    assert_eq!(
        select_replay_parts(parts, 4096, true, true)
            .unwrap()
            .as_deref(),
        Some(parts)
    );
    assert_eq!(select_replay_parts(parts, 4096, true, false).unwrap(), None);
    assert_eq!(
        select_replay_parts(parts, 4096, false, false).unwrap(),
        None
    );
}

#[test]
fn responses_replay_projection_rejects_invalid_and_oversized_state() {
    for invalid in [
        "{}",
        "not json",
        r#"[{"type":"function_call"}]"#,
        r#"[{"kind":"reasoning"}]"#,
        r#"["reasoning"]"#,
    ] {
        assert_eq!(
            select_replay_parts(invalid, 4096, false, true),
            Err(ResponsesError::InvalidProviderState),
            "{invalid}"
        );
    }
    let parts = r#"[{"type":"reasoning","encrypted_content":"kept"}]"#;
    assert_eq!(
        select_replay_parts(parts, parts.len() - 1, false, true),
        Err(ResponsesError::ProviderStateTooLarge)
    );
}

#[test]
fn responses_request_preserves_assistant_commentary_phase() {
    let messages = [
        assistant(
            Some("I will inspect the file first."),
            vec![call("call_1", "read_file", "{}")],
        ),
        tool_result("call_1", "contents"),
    ];
    let items = input(
        &messages,
        &[Some(r#"[{"type":"message","phase":"commentary"}]"#), None],
    )
    .unwrap();
    assert_eq!(items[0]["type"], "message");
    assert_eq!(items[0]["phase"], "commentary");
    assert_eq!(
        items[0].to_string(),
        r#"{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"I will inspect the file first.","annotations":[]}],"phase":"commentary"}"#
    );
}

#[test]
fn non_object_function_arguments_cannot_enter_a_responses_request() {
    for arguments in [
        "[]",
        "42",
        "null",
        "true",
        "\"text\"",
        "{]",
        r#"{"a":1,"a":2}"#,
    ] {
        let messages = [assistant(None, vec![call("call", "read_file", arguments)])];
        assert_eq!(
            input(&messages, &[None]).unwrap_err(),
            ResponsesError::InvalidToolArguments,
            "{arguments}"
        );
    }
}

#[test]
fn objects_the_upstream_parser_accepts_enter_a_responses_request_unchanged() {
    let deep = format!("{{\"a\":{}{}}}", "[".repeat(200), "]".repeat(200));
    for arguments in [
        "{}",
        r#"{"limit":1e999}"#,
        r#"{"offset":123456789012345678901234567890}"#,
        deep.as_str(),
    ] {
        let messages = [
            assistant(None, vec![call("call", "read_file", arguments)]),
            tool_result("call", "result"),
        ];
        let items = input(&messages, &[None]).unwrap();
        assert_eq!(items[0]["arguments"], arguments);
    }
}

#[test]
fn responses_requests_reject_system_messages_and_oversized_replays() {
    assert_eq!(
        input(
            &[ChatMessage::System {
                content: "x".to_owned()
            }],
            &[None]
        )
        .unwrap_err(),
        ResponsesError::InvalidProviderPrompt
    );
    let state = format!(
        r#"[{{"type":"reasoning","encrypted_content":"{}"}}]"#,
        "x".repeat(4096)
    );
    assert_eq!(
        input(&[assistant(Some("a"), Vec::new())], &[Some(&state)]).unwrap_err(),
        ResponsesError::ProviderStateTooLarge
    );
    let calls = (0..129).map(|_| call("call", "read", "{}")).collect();
    assert_eq!(
        input(&[assistant(None, calls)], &[None]).unwrap_err(),
        ResponsesError::ToolCallLimitExceeded
    );
}

#[test]
fn responses_absent_final_snapshot_preserves_completed_stream_evidence() {
    for snapshot in ["", r#","output":[]"#, r#","output":null"#] {
        let mut stream = Stream::new();
        stream.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_done","phase":"final_answer","content":[{"type":"output_text","text":"Completed answer."}]}}"#).unwrap();
        stream
            .apply(&format!(
                r#"{{"type":"response.completed","response":{{"status":"completed"{snapshot}}}}}"#
            ))
            .unwrap();
        let completion = stream.finish().unwrap();
        assert_eq!(completion.content.as_deref(), Some("Completed answer."));
        assert_eq!(completion.finish, ResponsesFinish::Stop);
        assert!(completion.provider_state.is_some());
    }
}

#[test]
fn responses_output_slot_cannot_change_kind_before_completion() {
    for event in [
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_replacement","content":[{"type":"output_text","text":"conflicting text"}]}}"#,
        r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","id":"msg_replacement","content":[{"type":"output_text","text":"conflicting text"}]}]}}"#,
    ] {
        let mut stream = Stream::new();
        stream.apply(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_original","call_id":"call_original","name":"read_file","arguments":"{}"}}"#).unwrap();
        assert_eq!(stream.apply(event), Err(ResponsesError::OutputItemConflict));
        assert_eq!(stream.emitted, "");
        assert_eq!(
            stream.finish().unwrap_err(),
            ResponsesError::StreamIncomplete
        );
    }
}

#[test]
fn responses_output_kinds_remain_exclusive_across_item_event_stages() {
    let items = [
        r#"{"type":"message","content":[]}"#,
        r#"{"type":"reasoning"}"#,
        r#"{"type":"function_call","id":"fc","call_id":"call","name":"read_file","arguments":"{}"}"#,
        r#"{"type":"future_item"}"#,
    ];
    for (from, initial) in items[..3].iter().enumerate() {
        for (to, replacement) in items.iter().enumerate() {
            for stage in [
                "response.output_item.added",
                "response.output_item.done",
                "response.completed",
            ] {
                let mut stream = Stream::new();
                stream
                    .apply(&format!(
                        r#"{{"type":"response.output_item.added","output_index":0,"item":{initial}}}"#
                    ))
                    .unwrap();
                let event = if stage == "response.completed" {
                    format!(
                        r#"{{"type":"response.completed","response":{{"status":"completed","output":[{replacement}]}}}}"#
                    )
                } else {
                    format!(r#"{{"type":"{stage}","output_index":0,"item":{replacement}}}"#)
                };
                if from == to {
                    stream.apply(&event).unwrap();
                    stream.apply(TERMINAL).unwrap();
                    stream.finish().unwrap();
                } else {
                    assert_eq!(
                        stream.apply(&event),
                        Err(ResponsesError::OutputItemConflict)
                    );
                    assert_eq!(
                        stream.finish().unwrap_err(),
                        ResponsesError::StreamIncomplete
                    );
                }
            }
        }
    }
}

#[test]
fn responses_typed_deltas_cannot_target_a_different_owned_kind() {
    let cases = [
        (
            START,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"wrong"}"#,
        ),
        (
            START,
            r#"{"type":"response.refusal.done","output_index":0,"refusal":"wrong"}"#,
        ),
        (
            START,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"wrong"}"#,
        ),
        (
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}"#,
        ),
        (
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning"}}"#,
            r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{}"}"#,
        ),
    ];
    for (start, event) in cases {
        let mut stream = Stream::new();
        stream.apply(start).unwrap();
        assert_eq!(stream.apply(event), Err(ResponsesError::OutputItemConflict));
        assert_eq!(stream.emitted, "");
    }
}

#[test]
fn responses_non_null_snapshot_shapes_remain_invalid() {
    for snapshot in ["{}", "false", "0", "\"invalid\""] {
        let mut stream = Stream::new();
        assert_eq!(
            stream.apply(&format!(
                r#"{{"type":"response.completed","response":{{"status":"completed","output":{snapshot}}}}}"#
            )),
            Err(ResponsesError::InvalidEvent)
        );
        assert_eq!(
            stream.finish().unwrap_err(),
            ResponsesError::StreamIncomplete
        );
    }
}

#[test]
fn responses_null_snapshot_preserves_separate_item_kinds_without_extra_replay() {
    let mut stream = Stream::new();
    stream
        .apply(
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning"}}"#,
        )
        .unwrap();
    stream.apply(START).unwrap();
    stream.apply(FINALIZED).unwrap();
    let reasoning = r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","encrypted_content":"retained"}}"#;
    stream.apply(reasoning).unwrap();
    stream.apply(reasoning).unwrap();
    stream
        .apply(r#"{"type":"response.output_text.delta","output_index":2,"delta":"done"}"#)
        .unwrap();
    stream
        .apply(r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"future_item"}}"#)
        .unwrap();
    stream
        .apply(r#"{"type":"response.completed","response":{"status":"completed","output":null}}"#)
        .unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(completion.content.as_deref(), Some("done"));
    assert_eq!(completion.tool_calls.len(), 1);
    assert_eq!(completion.finish, ResponsesFinish::ToolCalls);
    assert_eq!(
        replay_of(&completion),
        Some(r#"[{"type":"reasoning","encrypted_content":"retained"}]"#)
    );
}

#[test]
fn responses_message_replay_preserves_separate_commentary_and_final_text() {
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","id":"msg_progress","phase":"commentary","content":[{"type":"output_text","text":"Checking."}]},{"type":"message","id":"msg_final","phase":"final_answer","content":[{"type":"output_text","text":"42"}]}]}}"#).unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(completion.content.as_deref(), Some("Checking.\n\n42"));
    let items = input(
        &[assistant(completion.content.as_deref(), Vec::new())],
        &[Some(replay_of(&completion).unwrap_or("[]"))],
    )
    .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["phase"], "commentary");
    assert_eq!(items[0]["content"][0]["text"], "Checking.");
    assert_eq!(items[1]["phase"], "final_answer");
    assert_eq!(items[1]["content"][0]["text"], "42");
}

#[test]
fn responses_reasoning_replay_retains_terminal_only_context() {
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque"}]}}"#).unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(
        replay_of(&completion),
        Some(r#"[{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque"}]"#)
    );
}

#[test]
fn responses_message_replay_rejects_ambiguous_or_invalid_spans() {
    for parts in [
        r#"[{"type":"message","offset":-1,"length":1}]"#,
        r#"[{"type":"message","offset":0,"length":6}]"#,
        r#"[{"type":"message","offset":6,"length":1}]"#,
        r#"[{"type":"message","offset":0,"length":0}]"#,
        r#"[{"type":"message","offset":0}]"#,
        r#"[{"type":"message","length":1}]"#,
        r#"[{"type":"message","offset":"0","length":1}]"#,
        r#"[{"type":"message","offset":0,"length":1,"phase":42}]"#,
        r#"[{"type":"message","offset":1,"length":4}]"#,
        r#"[{"type":"message","offset":0,"length":4},{"type":"message","offset":3,"length":2}]"#,
        r#"[{"type":"message","offset":0,"length":1},{"type":"message","offset":4,"length":1}]"#,
        r#"[{"type":"message","offset":0,"length":1}]"#,
        r#"[{"type":"message","phase":"commentary"},{"type":"message","offset":0,"length":5}]"#,
        r#"[{"type":"message","offset":0,"length":5},{"type":"message","phase":"commentary"}]"#,
    ] {
        assert_eq!(
            input(&[assistant(Some("a\n\nbc"), Vec::new())], &[Some(parts)]).unwrap_err(),
            ResponsesError::InvalidProviderState,
            "{parts}"
        );
    }
}

#[test]
fn responses_message_replay_binds_item_identity_before_text_and_across_content_parts() {
    for event in [
        r#"{"type":"response.output_text.delta","output_index":0,"item_id":"changed","delta":"bad"}"#,
        r#"{"type":"response.output_text.delta","output_index":1,"item_id":"original","delta":"bad"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"changed","content":[]}}"#,
    ] {
        let mut stream = Stream::new();
        stream.apply(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"original","phase":"commentary"}}"#).unwrap();
        assert_eq!(stream.apply(event), Err(ResponsesError::TextConflict));
        assert_eq!(stream.emitted, "");
    }
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"original","delta":"a"}"#).unwrap();
    assert_eq!(
        stream.apply(r#"{"type":"response.output_text.delta","output_index":0,"content_index":1,"item_id":"changed","delta":"b"}"#),
        Err(ResponsesError::TextConflict)
    );
    assert_eq!(stream.emitted, "a");
}

#[test]
fn responses_reasoning_replay_retains_terminal_enrichment_without_duplicates() {
    for encrypted in ["", r#","encrypted_content":"opaque""#] {
        let mut stream = Stream::new();
        stream
            .apply(&format!(
                r#"{{"type":"response.output_item.done","output_index":0,"item":{{"id":"rs_1","type":"reasoning","summary":[]{encrypted}}}}}"#
            ))
            .unwrap();
        stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque"}]}}"#).unwrap();
        let completion = stream.finish().unwrap();
        assert_eq!(
            replay_of(&completion),
            Some(r#"[{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque"}]"#)
        );
    }
}

#[test]
fn responses_reasoning_replay_orders_sparse_items_and_ignores_equivalent_duplicates() {
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.output_item.done","output_index":9223372036854775807,"item":{"type":"reasoning","encrypted_content":"later"}}"#).unwrap();
    stream.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"first"}}"#).unwrap();
    stream.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"encrypted_content":"first","type":"reasoning"}}"#).unwrap();
    stream.apply(TERMINAL).unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(
        replay_of(&completion),
        Some(
            r#"[{"type":"reasoning","encrypted_content":"first"},{"type":"reasoning","encrypted_content":"later"}]"#
        )
    );
}

#[test]
fn responses_reasoning_replay_binds_supplied_identity_before_ciphertext() {
    for kind in ["response.output_item.added", "response.output_item.done"] {
        let mut stream = Stream::new();
        stream
            .apply(&format!(
                r#"{{"type":"{kind}","output_index":0,"item":{{"type":"reasoning","id":"rs_original"}}}}"#
            ))
            .unwrap();
        assert_eq!(
            stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","id":"rs_replacement","encrypted_content":"opaque"}]}}"#),
            Err(ResponsesError::ReasoningConflict)
        );
    }
}

#[test]
fn responses_reasoning_replay_rejects_supplied_identity_at_another_position() {
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_same","encrypted_content":"opaque"}}"#).unwrap();
    assert_eq!(
        stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","id":"rs_same","encrypted_content":"opaque"}]}}"#),
        Err(ResponsesError::ReasoningConflict)
    );
}

#[test]
fn responses_reasoning_replay_omits_identity_only_items_and_bounds_their_count() {
    let mut stream = Stream::new();
    stream
        .apply(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_empty"}}"#)
        .unwrap();
    stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","id":"rs_empty","encrypted_content":null},{"type":"reasoning","id":"rs_full","encrypted_content":"opaque"}]}}"#).unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(
        replay_of(&completion),
        Some(r#"[{"type":"reasoning","id":"rs_full","encrypted_content":"opaque"}]"#)
    );

    let mut bounded = Stream::with_limits(StreamLimits {
        events: 1,
        ..LIMITS
    });
    assert_eq!(
        bounded.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","id":"a"},{"type":"reasoning","id":"b"}]}}"#),
        Err(ResponsesError::ResourceLimitExceeded)
    );
}

#[test]
fn responses_reasoning_replay_keeps_the_first_ciphertext_when_a_later_copy_is_reencrypted() {
    let kept = r#"{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"first"}"#;
    for later in [
        r#"{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"second"}"#,
        r#"{"encrypted_content":"third","summary":[],"type":"reasoning","id":"rs_1"}"#,
        r#"{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":null}"#,
        r#"{"id":"rs_1","type":"reasoning","summary":[]}"#,
    ] {
        let mut stream = Stream::new();
        stream
            .apply(&format!(
                r#"{{"type":"response.output_item.done","output_index":0,"item":{kept}}}"#
            ))
            .unwrap();
        stream
            .apply(&format!(
                r#"{{"type":"response.output_item.done","output_index":0,"item":{later}}}"#
            ))
            .unwrap();
        stream
            .apply(&format!(
                r#"{{"type":"response.completed","response":{{"status":"completed","output":[{later}]}}}}"#
            ))
            .unwrap();
        let completion = stream.finish().unwrap();
        assert_eq!(replay_of(&completion), Some(format!("[{kept}]").as_str()));
    }
}

#[test]
fn responses_reasoning_replay_compares_nested_fields_regardless_of_key_order() {
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"plan"}],"encrypted_content":"first"}}"#).unwrap();
    stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"summary":[{"text":"plan","type":"summary_text"}],"encrypted_content":"second","type":"reasoning","id":"rs_1"}]}}"#).unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(
        replay_of(&completion),
        Some(
            r#"[{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"plan"}],"encrypted_content":"first"}]"#
        )
    );
}

#[test]
fn responses_reasoning_replay_treats_an_empty_ciphertext_as_none() {
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":""}}"#).unwrap();
    stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"real"}]}}"#).unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(
        replay_of(&completion),
        Some(r#"[{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"real"}]"#)
    );

    let mut empty = Stream::new();
    empty.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":""}}"#).unwrap();
    empty.apply(TERMINAL).unwrap();
    assert_eq!(replay_of(&empty.finish().unwrap()), None);
}

#[test]
fn responses_reasoning_replay_keeps_the_streamed_copy_of_a_live_reencrypted_item() {
    let live = include_str!("live_reencrypted_reasoning.sse");
    let events: Vec<&str> = live
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .collect();
    let reasoning_copy = |kind: &str| -> Value {
        events
            .iter()
            .map(|event| serde_json::from_str::<Value>(event).unwrap())
            .find_map(|event| match event["type"].as_str() {
                Some(found) if found == kind && event["item"]["type"] == "reasoning" => {
                    Some(event["item"].clone())
                }
                Some("response.completed") if kind == "response.completed" => {
                    Some(event["response"]["output"][0].clone())
                }
                _ => None,
            })
            .unwrap()
    };
    let added = reasoning_copy("response.output_item.added");
    let done = reasoning_copy("response.output_item.done");
    let completed = reasoning_copy("response.completed");
    let ciphertexts = [&added, &done, &completed].map(|item| item["encrypted_content"].clone());
    assert!(ciphertexts.iter().all(Value::is_string));
    assert_ne!(ciphertexts[0], ciphertexts[1]);
    assert_ne!(ciphertexts[1], ciphertexts[2]);
    assert_ne!(ciphertexts[0], ciphertexts[2]);
    assert_eq!(done["id"], completed["id"]);
    assert_eq!(done["summary"], completed["summary"]);

    let mut stream = Stream::new();
    for event in &events {
        stream.apply(event).unwrap();
    }
    let answer =
        "When both `text` and `reasoning` are false, `select_replay_parts` returns `Ok(None)`.";
    assert_eq!(stream.emitted, answer);
    let completion = stream.finish().unwrap();
    assert_eq!(completion.content.as_deref(), Some(answer));
    assert_eq!(completion.finish, ResponsesFinish::Stop);
    assert_eq!(completion.usage.input_tokens, Some(2967));
    assert_eq!(completion.usage.output_tokens, Some(79));
    let state: Value = serde_json::from_str(replay_of(&completion).unwrap()).unwrap();
    assert_eq!(
        state,
        json!([
            done,
            {"type": "message", "offset": 0, "length": answer.len(), "phase": "final_answer"},
        ])
    );
}

#[test]
fn responses_reasoning_replay_rejects_conflicting_final_evidence_and_invalid_supplied_identity() {
    for item in [
        r#"{"id":"different","type":"reasoning","encrypted_content":"opaque"}"#,
        r#"{"type":"reasoning","encrypted_content":"different"}"#,
        r#"{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"other"}],"encrypted_content":"different"}"#,
        r#"{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"other"}],"encrypted_content":"opaque"}"#,
        r#"{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"other"}]}"#,
        r#"{"id":"rs_1","type":"reasoning","summary":[],"status":"completed","encrypted_content":"different"}"#,
        r#"{"id":"rs_1","type":"reasoning","encrypted_content":"different"}"#,
        r#"{"id":"rs_1","type":"reasoning","summary":[],"index":1.0,"encrypted_content":"different"}"#,
    ] {
        for streamed in [
            r#"{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque"}"#,
            r#"{"id":"rs_1","type":"reasoning","summary":[]}"#,
            r#"{"id":"rs_1","type":"reasoning","summary":[],"index":1}"#,
        ] {
            let mut stream = Stream::new();
            stream
                .apply(&format!(
                    r#"{{"type":"response.output_item.done","output_index":0,"item":{streamed}}}"#
                ))
                .unwrap();
            assert_eq!(
                stream.apply(&format!(
                    r#"{{"type":"response.completed","response":{{"status":"completed","output":[{item}]}}}}"#
                )),
                Err(ResponsesError::ReasoningConflict),
                "{streamed} then {item}"
            );
            assert_eq!(
                stream.finish().unwrap_err(),
                ResponsesError::StreamIncomplete
            );
        }
    }
    for item in [
        r#"{"id":42,"type":"reasoning","encrypted_content":"opaque"}"#,
        r#"{"id":"","type":"reasoning","encrypted_content":"opaque"}"#,
        r#"{"type":"reasoning","encrypted_content":42}"#,
    ] {
        let mut stream = Stream::new();
        assert_eq!(
            stream.apply(&format!(
                r#"{{"type":"response.completed","response":{{"status":"completed","output":[{item}]}}}}"#
            )),
            Err(ResponsesError::InvalidEvent)
        );
    }
}

#[test]
fn responses_reasoning_replay_counts_unique_bytes_and_phase_at_the_exact_bound() {
    let event = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"opaque"}}"#;
    let state = r#"[{"type":"reasoning","encrypted_content":"opaque"}]"#;
    for limit in [state.len(), state.len() - 1] {
        let mut stream = Stream::with_limits(StreamLimits {
            provider_state_bytes: limit,
            ..LIMITS
        });
        if limit < state.len() {
            assert_eq!(
                stream.apply(event),
                Err(ResponsesError::ResourceLimitExceeded)
            );
            continue;
        }
        for _ in 0..3 {
            stream.apply(event).unwrap();
        }
        stream.apply(TERMINAL).unwrap();
        assert_eq!(replay_of(&stream.finish().unwrap()), Some(state));
    }
    let phase = r#"{"type":"message","offset":0,"length":1,"phase":"commentary"}"#;
    for limit in [state.len() + phase.len() + 1, state.len() + phase.len()] {
        let mut stream = Stream::with_limits(StreamLimits {
            provider_state_bytes: limit,
            ..LIMITS
        });
        stream.apply(event).unwrap();
        stream.apply(r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","phase":"commentary"}}"#).unwrap();
        stream
            .apply(r#"{"type":"response.output_text.delta","output_index":1,"delta":"x"}"#)
            .unwrap();
        stream.apply(TERMINAL).unwrap();
        if limit == state.len() + phase.len() {
            assert_eq!(
                stream.finish().unwrap_err(),
                ResponsesError::ResourceLimitExceeded
            );
        } else {
            assert_eq!(
                replay_of(&stream.finish().unwrap()).map(str::len),
                Some(limit)
            );
        }
    }
}

#[test]
fn responses_text_finalization_preserves_mixed_streamed_and_final_only_items() {
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_0","delta":"COMMENTARY_ITEM\n"}"#).unwrap();
    stream.apply(r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"FINAL_ANSWER_ITEM"}]}}"#).unwrap();
    stream.apply(TERMINAL).unwrap();
    assert_eq!(
        stream.finish().unwrap().content.as_deref(),
        Some("COMMENTARY_ITEM\n\n\nFINAL_ANSWER_ITEM")
    );
}

#[test]
fn responses_terminal_failures_retain_provider_diagnostics_as_outcomes() {
    for event in [
        r#"{"type":"response.failed","response":{"id":"resp_failed","status":"failed","error":{"code":"server_error","message":"temporarily unavailable"}}}"#,
        r#"{"type":"error","code":"server_error","message":"temporarily unavailable"}"#,
    ] {
        let mut stream = Stream::new();
        stream.apply(event).unwrap();
        let completion = stream.finish().unwrap();
        assert_eq!(completion.finish, ResponsesFinish::ProviderError);
        assert_eq!(
            completion.failure,
            Some(ProviderFailure {
                code: "server_error".to_owned(),
                message: "temporarily unavailable".to_owned(),
                cause: FailureCause::Retryable,
            })
        );
        assert_eq!(
            completion.failure.unwrap().detail(|text| text),
            "server_error: temporarily unavailable"
        );
    }
}

#[test]
fn responses_error_events_read_a_nested_error_object_only_when_the_top_level_is_empty() {
    for (event, code, message, cause) in [
        (
            r#"{"type":"error","error":{"type":"server_error","code":null,"message":"The server had an error while processing your request.","param":null},"sequence_number":4}"#,
            "server_error",
            "The server had an error while processing your request.",
            FailureCause::Retryable,
        ),
        (
            r#"{"type":"error","error":{"type":"tokens","code":"rate_limit_exceeded","message":"Rate limit reached."}}"#,
            "rate_limit_exceeded",
            "Rate limit reached.",
            FailureCause::RateLimited,
        ),
        (
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Bad input."}}"#,
            "invalid_request_error",
            "Bad input.",
            FailureCause::NonRetryable,
        ),
        (
            r#"{"type":"error","code":"server_error","message":"flat","error":{"code":"rate_limit_exceeded","message":"nested"}}"#,
            "server_error",
            "flat",
            FailureCause::Retryable,
        ),
        (
            r#"{"type":"error","message":"flat","error":{"code":"rate_limit_exceeded","message":"nested"}}"#,
            "provider_error",
            "flat",
            FailureCause::NonRetryable,
        ),
        (
            r#"{"type":"error","error":"server_error"}"#,
            "provider_error",
            "Provider response failed",
            FailureCause::NonRetryable,
        ),
        (
            r#"{"type":"error"}"#,
            "provider_error",
            "Provider response failed",
            FailureCause::NonRetryable,
        ),
    ] {
        let mut stream = Stream::new();
        stream.apply(event).unwrap();
        let completion = stream.finish().unwrap();
        assert_eq!(completion.finish, ResponsesFinish::ProviderError, "{event}");
        assert_eq!(
            completion.failure,
            Some(ProviderFailure {
                code: code.to_owned(),
                message: message.to_owned(),
                cause,
            }),
            "{event}"
        );
    }
}

#[test]
fn responses_terminal_incomplete_event_does_not_require_nested_status() {
    let mut stream = Stream::new();
    stream
        .apply(r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#)
        .unwrap();
    assert_eq!(stream.finish().unwrap().finish, ResponsesFinish::Length);
}

#[test]
fn responses_terminal_failure_classification_is_conservative_and_diagnostics_are_bounded() {
    for (code, cause) in [
        ("server_error", FailureCause::Retryable),
        ("rate_limit_exceeded", FailureCause::RateLimited),
        ("invalid_prompt", FailureCause::NonRetryable),
        ("unknown_code", FailureCause::NonRetryable),
    ] {
        let mut stream = Stream::new();
        stream
            .apply(
                &json!({
                    "type": "response.failed",
                    "response": {
                        "id": "resp_failure",
                        "error": {"code": code, "message": "é".repeat(512)},
                        "usage": {"input_tokens": 7, "output_tokens": 3}
                    }
                })
                .to_string(),
            )
            .unwrap();
        let completion = stream.finish().unwrap();
        let failure = completion.failure.unwrap();
        assert_eq!(failure.cause, cause);
        let detail = failure.detail(|text| text);
        assert!(detail.len() <= 256);
        assert!(detail.starts_with(code));
        assert_eq!(completion.usage.input_tokens, Some(7));
        assert_eq!(completion.usage.output_tokens, Some(3));
    }
    let mut stream = Stream::new();
    stream
        .apply(r#"{"type":"response.failed","response":{"error":null}}"#)
        .unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(completion.finish, ResponsesFinish::ProviderError);
    let failure = completion.failure.unwrap();
    assert_eq!(failure.cause, FailureCause::NonRetryable);
    assert!(!failure.detail(|text| text).is_empty());
}

#[test]
fn responses_terminal_metadata_rejects_contradictions_before_publishing_final_text() {
    for event in [
        r#"{"type":"response.completed","response":{"status":"incomplete","output":[{"type":"message","content":[{"type":"output_text","text":"must not publish"}]}]}}"#,
        r#"{"type":"response.incomplete","response":{"status":"completed"}}"#,
        r#"{"type":"response.failed","response":{"status":42}}"#,
        r#"{"type":"response.done","response":{"status":"in_progress"}}"#,
    ] {
        let mut stream = Stream::new();
        assert_eq!(stream.apply(event), Err(ResponsesError::InvalidEvent));
        assert_eq!(stream.emitted, "");
    }
}

#[test]
fn responses_terminal_failure_retains_progress_without_final_only_output() {
    let mut stream = Stream::new();
    stream.delta(0, 0, "partial").unwrap();
    stream.apply(r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":""}}"#).unwrap();
    stream.apply(r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"retry"},"output":[{"type":"message","content":[{"type":"output_text","text":"must not publish"}]}]}}"#).unwrap();
    assert_eq!(stream.emitted, "partial");
    let completion = stream.finish().unwrap();
    assert_eq!(completion.content.as_deref(), Some("partial"));
    assert_eq!(completion.tool_calls.len(), 1);
    assert_eq!(completion.finish, ResponsesFinish::ProviderError);
}

#[test]
fn responses_text_finalization_converges_across_every_final_record_layer() {
    let mut stream = Stream::new();
    stream.delta(0, 0, "Hel").unwrap();
    stream.final_text(0, 0, "Hello").unwrap();
    stream.apply(r#"{"type":"response.content_part.done","output_index":0,"content_index":0,"item_id":"msg","part":{"type":"output_text","text":"Hello"}}"#).unwrap();
    stream.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"msg","type":"message","content":[{"type":"output_text","text":"Hello"},{"type":"refusal","refusal":"!"}]}}"#).unwrap();
    stream.apply(r#"{"type":"response.refusal.done","output_index":0,"content_index":1,"item_id":"msg","refusal":"!"}"#).unwrap();
    stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","id":"msg","content":[{"type":"output_text","text":"Hello"},{"type":"refusal","refusal":"!"}]}]}}"#).unwrap();
    stream.finish_text("Hello!");
}

#[test]
fn responses_text_finalization_reads_terminal_only_messages_and_streamed_refusals() {
    let mut stream = Stream::new();
    stream
        .apply(r#"{"type":"response.refusal.delta","output_index":0,"delta":"No"}"#)
        .unwrap();
    stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"No."}]},{"type":"message","content":[{"type":"output_text","text":" Alternative."}]}]}}"#).unwrap();
    stream.finish_text("No.\n\n Alternative.");
}

#[test]
fn responses_text_finalization_retains_item_text_when_the_terminal_envelope_is_empty() {
    let mut stream = Stream::new();
    stream.delta(0, 0, "accepted").unwrap();
    stream.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"message","type":"message","content":[{"type":"output_text","text":"accepted final"}]}}"#).unwrap();
    stream
        .apply(r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#)
        .unwrap();
    stream.finish_text("accepted final");
}

#[test]
fn responses_text_finalization_rejects_contradictory_content_identity_and_finality() {
    for final_record in [
        r#"{"type":"response.output_text.done","text":"changed","item_id":"a"}"#,
        r#"{"type":"response.output_text.done","text":"he","item_id":"a"}"#,
        r#"{"type":"response.output_text.done","text":"hello","item_id":"b"}"#,
        r#"{"type":"response.refusal.done","refusal":"hello","item_id":"a"}"#,
    ] {
        let mut stream = Stream::new();
        stream
            .apply(r#"{"type":"response.output_text.delta","delta":"hello","item_id":"a"}"#)
            .unwrap();
        assert_eq!(
            stream.apply(final_record),
            Err(ResponsesError::TextConflict)
        );
        assert_eq!(stream.emitted, "hello");
    }
    let mut stream = Stream::new();
    stream.final_text(0, 0, "done").unwrap();
    assert_eq!(
        stream.final_text(0, 0, "done later"),
        Err(ResponsesError::TextConflict)
    );
    assert_eq!(
        stream.delta(0, 0, "later"),
        Err(ResponsesError::TextConflict)
    );
    stream.delta(0, 0, "").unwrap();
    stream.finish_text("done");
}

#[test]
fn responses_text_finalization_rejects_malformed_supplied_correlation() {
    for event in [
        r#"{"type":"response.output_text.delta","output_index":-1,"delta":"a"}"#,
        r#"{"type":"response.output_text.delta","content_index":"0","delta":"a"}"#,
        r#"{"type":"response.output_text.done","content_index":null,"text":"a"}"#,
        r#"{"type":"response.output_text.done","item_id":7,"text":"a"}"#,
        r#"{"type":"response.output_text.done","item_id":"","text":"a"}"#,
        r#"{"type":"response.output_text.done","text":7}"#,
        r#"{"type":"response.content_part.done","part":null}"#,
    ] {
        let mut stream = Stream::new();
        assert_eq!(
            stream.apply(event),
            Err(ResponsesError::InvalidEvent),
            "{event}"
        );
        assert_eq!(stream.emitted, "");
    }
}

#[test]
fn responses_text_finalization_preserves_append_order_and_bounded_sparse_indexes() {
    let mut stream = Stream::new();
    stream.delta(0, 0, "a").unwrap();
    stream.final_text(99_999_999, 99_999_999, "b").unwrap();
    stream.final_text(0, 0, "a").unwrap();
    assert_eq!(
        stream.final_text(1, 0, "late"),
        Err(ResponsesError::TextConflict)
    );
    assert_eq!(stream.emitted, "a\n\nb");

    let mut bounded = Stream::with_limits(StreamLimits {
        events: 2,
        ..LIMITS
    });
    assert_eq!(
        bounded.apply(r#"{"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"a"},{"type":"output_text","text":"b"},{"type":"output_text","text":"c"}]}]}}"#),
        Err(ResponsesError::ResourceLimitExceeded)
    );
    assert_eq!(bounded.emitted, "ab");
}

#[test]
fn responses_captures_assistant_commentary_phase() {
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","phase":"commentary"}}"#).unwrap();
    stream
        .apply(r#"{"type":"response.output_text.delta","output_index":0,"delta":"I will inspect the file first."}"#)
        .unwrap();
    stream.apply(TERMINAL).unwrap();
    let completion = stream.finish().unwrap();
    let replay: Value = serde_json::from_str(replay_of(&completion).unwrap()).unwrap();
    assert_eq!(replay[0]["phase"], "commentary");
}

#[test]
fn responses_captures_assistant_phase_from_terminal_output() {
    let mut stream = Stream::new();
    stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Finished."}]}]}}"#).unwrap();
    let completion = stream.finish().unwrap();
    let replay: Value = serde_json::from_str(replay_of(&completion).unwrap()).unwrap();
    assert_eq!(replay[0]["phase"], "final_answer");
}

#[test]
fn responses_omits_unknown_phases_and_rejects_contradictory_phases_within_one_message() {
    let mut unknown = Stream::new();
    unknown.apply(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","phase":"future_phase"}}"#).unwrap();
    unknown.apply(TERMINAL).unwrap();
    assert_eq!(unknown.finish().unwrap().provider_state, None);

    let mut conflicting = Stream::new();
    conflicting.apply(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","phase":"commentary"}}"#).unwrap();
    assert_eq!(
        conflicting.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","phase":"final_answer","content":[]}}"#),
        Err(ResponsesError::TextConflict)
    );
}

#[test]
fn responses_rejects_conflicting_completed_tool_records() {
    for event in [
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"preview.txt\"}"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"other","name":"write_file","arguments":"{\"path\":\"preview.txt\"}"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"other","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"preview.txt\"}"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"final.txt\"}"}}"#,
    ] {
        let mut stream = Stream::new();
        stream.apply(START).unwrap();
        stream.apply(FINALIZED).unwrap();
        assert_eq!(stream.apply(event), Err(ResponsesError::ToolCallConflict));
    }
}

#[test]
fn responses_completed_item_replaces_progressive_arguments() {
    let mut stream = Stream::new();
    stream.apply(START).unwrap();
    stream.apply(r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"}"#).unwrap();
    stream.apply(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"final.txt\"}"}}"#).unwrap();
    stream.apply(TERMINAL).unwrap();
    assert_eq!(
        stream.finish().unwrap().tool_calls[0].arguments,
        r#"{"path":"final.txt"}"#
    );
}

#[test]
fn responses_completed_response_validates_supplied_tool_snapshots() {
    let mut stream = Stream::new();
    stream.apply(START).unwrap();
    stream.apply(FINALIZED).unwrap();
    assert_eq!(
        stream.apply(r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"final.txt\"}"}]}}"#),
        Err(ResponsesError::ToolCallConflict)
    );
}

#[test]
fn responses_completed_response_can_finalize_progressive_arguments_within_the_byte_limit() {
    for size in [LIMITS.tool_arguments_bytes, LIMITS.tool_arguments_bytes + 1] {
        let mut stream = Stream::new();
        stream.apply(START).unwrap();
        stream
            .apply(
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{"}"#,
            )
            .unwrap();
        let arguments = format!("{{{}}}", " ".repeat(size - 2));
        let event = json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "write_file",
                "arguments": arguments,
            }]},
        })
        .to_string();
        if size > LIMITS.tool_arguments_bytes {
            assert_eq!(
                stream.apply(&event),
                Err(ResponsesError::ToolArgumentsTooLarge)
            );
        } else {
            stream.apply(&event).unwrap();
            assert_eq!(stream.finish().unwrap().tool_calls[0].arguments, arguments);
        }
    }
}

#[test]
fn responses_finalized_arguments_cannot_receive_more_deltas() {
    let mut stream = Stream::new();
    stream.apply(START).unwrap();
    stream.apply(FINALIZED).unwrap();
    assert_eq!(
        stream.apply(
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":" "}"#
        ),
        Err(ResponsesError::ToolCallConflict)
    );
}

#[test]
fn responses_equivalent_finalized_records_retain_one_accepted_argument_representation() {
    let mut stream = Stream::new();
    stream.apply(START).unwrap();
    stream.apply(FINALIZED).unwrap();
    let equivalent = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":" { \"path\" : \"preview.txt\" } "}}"#;
    stream.apply(equivalent).unwrap();
    stream.apply(equivalent).unwrap();
    stream.apply(FINALIZED).unwrap();
    stream.apply(TERMINAL).unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(completion.tool_calls.len(), 1);
    assert_eq!(
        completion.tool_calls[0].arguments,
        r#"{"path":"preview.txt"}"#
    );
}

#[test]
fn responses_final_argument_evidence_does_not_manufacture_an_empty_object() {
    for (event, arguments) in [
        (
            r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":""}"#,
            "",
        ),
        (
            r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"[]"}"#,
            "[]",
        ),
        (
            r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{]"}"#,
            "{]",
        ),
    ] {
        let mut stream = Stream::new();
        stream.apply(START).unwrap();
        stream
            .apply(
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{"}"#,
            )
            .unwrap();
        stream.apply(event).unwrap();
        stream.apply(TERMINAL).unwrap();
        assert_eq!(stream.finish().unwrap().tool_calls[0].arguments, arguments);
    }
}

#[test]
fn responses_interleaved_calls_keep_independent_finalization() {
    let mut stream = Stream::new();
    stream.apply(START).unwrap();
    stream.apply(r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"read_file","arguments":"{"}}"#).unwrap();
    stream.apply(r#"{"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_2","delta":"\"path\":\"second.txt\"}"}"#).unwrap();
    stream.apply(FINALIZED).unwrap();
    stream.apply(r#"{"type":"response.function_call_arguments.done","output_index":1,"item_id":"fc_2","name":"read_file","arguments":"{\"path\":\"second.txt\"}"}"#).unwrap();
    stream.apply(TERMINAL).unwrap();
    let completion = stream.finish().unwrap();
    assert_eq!(completion.tool_calls.len(), 2);
    assert_eq!(
        completion.tool_calls[0].arguments,
        r#"{"path":"preview.txt"}"#
    );
    assert_eq!(
        completion.tool_calls[1].arguments,
        r#"{"path":"second.txt"}"#
    );
}

#[test]
fn responses_finalization_checks_correlation_types_and_rejects_unmatched_final_calls() {
    for (event, failure) in [
        (
            r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"other","arguments":"{}"}"#,
            ResponsesError::ToolCallConflict,
        ),
        (
            r#"{"type":"response.function_call_arguments.done","output_index":0,"name":"read_file","arguments":"{}"}"#,
            ResponsesError::ToolCallConflict,
        ),
        (
            r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":null,"arguments":"{}"}"#,
            ResponsesError::InvalidEvent,
        ),
        (
            r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":{}}"#,
            ResponsesError::InvalidEvent,
        ),
        (
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":null,"arguments":"{}"}}"#,
            ResponsesError::InvalidEvent,
        ),
        (
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_2","arguments":"{}"}}"#,
            ResponsesError::ToolCallConflict,
        ),
    ] {
        let mut stream = Stream::new();
        stream.apply(START).unwrap();
        assert_eq!(stream.apply(event), Err(failure), "{event}");
    }
}

#[test]
fn responses_rejections_carry_the_type_only_of_an_event_that_was_parsed() {
    let mut reducer = Reducer::new(StreamLimits {
        events: 2,
        ..LIMITS
    });
    let (semantic, _) = reducer.apply(br#"{"type":"response.output_text.delta","delta":5}"#, false);
    assert_eq!(
        semantic,
        Err(Rejection {
            error: ResponsesError::InvalidEvent,
            event_type: Some("response.output_text.delta".to_owned()),
        })
    );
    let (malformed, _) = reducer.apply(br#"{"type":"response.completed""#, false);
    assert_eq!(malformed, Err(ResponsesError::InvalidEvent.into()));
    let (over_count, _) = reducer.apply(br#"{"type":"response.completed"}"#, false);
    assert_eq!(
        over_count,
        Err(ResponsesError::ResourceLimitExceeded.into())
    );

    let event = br#"{"type":"response.completed"}"#;
    let mut reducer = Reducer::new(StreamLimits {
        aggregate_bytes: event.len() - 1,
        ..LIMITS
    });
    let (over_bytes, _) = reducer.apply(event, false);
    assert_eq!(
        over_bytes,
        Err(ResponsesError::ResourceLimitExceeded.into())
    );
}

#[test]
fn responses_finalization_retains_cancellation_and_terminal_requirements() {
    let mut stream = Stream::new();
    stream.apply(START).unwrap();
    stream.apply(FINALIZED).unwrap();
    let (cancelled, _) = stream.reducer.apply(TERMINAL.as_bytes(), true);
    assert_eq!(cancelled, Err(ResponsesError::Cancelled.into()));
    assert_eq!(
        Stream::new().finish().unwrap_err(),
        ResponsesError::StreamIncomplete
    );
    assert_eq!(
        stream.reducer.finish(true).unwrap_err(),
        ResponsesError::Cancelled
    );
}

#[test]
fn responses_rejects_malformed_supplied_output_indexes_without_requiring_omitted_metadata() {
    for event in [
        r#"{"type":"response.function_call_arguments.done","output_index":"0","arguments":"{}"}"#,
        r#"{"type":"response.output_item.done","output_index":null,"item":{"type":"function_call","arguments":"{}"}}"#,
        r#"{"type":"response.output_item.added","output_index":-1,"item":{"type":"function_call","call_id":"bad","name":"read_file"}}"#,
        r#"{"type":"response.function_call_arguments.delta","output_index":0.0,"delta":"{}"}"#,
    ] {
        let mut stream = Stream::new();
        stream.apply(START).unwrap();
        stream.apply(FINALIZED).unwrap();
        assert_eq!(
            stream.apply(event),
            Err(ResponsesError::InvalidEvent),
            "{event}"
        );
    }
    let mut stream = Stream::new();
    stream.apply(START).unwrap();
    stream.apply(FINALIZED).unwrap();
    stream
        .apply(r#"{"type":"response.output_item.done","item":{"type":"function_call"}}"#)
        .unwrap();
    stream.apply(TERMINAL).unwrap();
    assert_eq!(
        stream.finish().unwrap().tool_calls[0].arguments,
        r#"{"path":"preview.txt"}"#
    );
}

#[test]
fn responses_finalization_preserves_the_length_finish_disposition() {
    let mut stream = Stream::new();
    stream.apply(START).unwrap();
    stream.apply(FINALIZED).unwrap();
    stream.apply(r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"function_call","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"preview.txt\"}"}]}}"#).unwrap();
    assert_eq!(stream.finish().unwrap().finish, ResponsesFinish::Length);
}

#[test]
fn responses_tools_serialize_typed_static_and_dynamic_functions_once() {
    let tools = [
        ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file.".to_owned(),
            input_schema:
                r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#
                    .into(),
        },
        ToolSpec {
            name: "mcp_search".to_owned(),
            description: "Search.".to_owned(),
            input_schema: r#"{"type":"object","properties":{"query":{"type":"string"}}}"#.into(),
        },
        ToolSpec {
            name: "read_file".to_owned(),
            description: String::new(),
            input_schema: r#"{"type":"object"}"#.into(),
        },
    ];
    let mut out = String::new();
    assert_eq!(write_tools(&mut out, &tools), Ok(2));
    assert_eq!(
        out,
        r#","tools":[{"type":"function","name":"read_file","description":"Read a file.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},"strict":false},{"type":"function","name":"mcp_search","description":"Search.","parameters":{"type":"object","properties":{"query":{"type":"string"}}},"strict":false}]"#
    );
    let mut empty = String::new();
    assert_eq!(write_tools(&mut empty, &[]), Ok(0));
    assert_eq!(empty, "");
    let invalid = ToolSpec {
        name: String::new(),
        description: String::new(),
        input_schema: r#"{"type":"object"}"#.into(),
    };
    assert_eq!(
        write_tools(&mut String::new(), &[invalid]),
        Err(ResponsesError::InvalidToolSchema)
    );
}

#[test]
fn responses_usage_projection_retains_optional_cached_and_reasoning_detail() {
    let response = json!({"usage": {"input_tokens": 17, "output_tokens": 7, "input_tokens_details": {"cached_tokens": 5}}});
    let usage = parse_usage(response.as_object().unwrap());
    assert_eq!(usage.input_tokens, Some(17));
    assert_eq!(usage.output_tokens, Some(7));
    let negative = json!({"usage": {"input_tokens": -1}});
    assert_eq!(
        parse_usage(negative.as_object().unwrap()).input_tokens,
        None
    );
}

#[test]
fn openai_codex_checked_stream_sizes_accept_the_bound_and_reject_overflow() {
    assert_eq!(checked_accumulated_size(6, 1, 7), Ok(7));
    assert_eq!(
        checked_accumulated_size(usize::MAX, 1, usize::MAX),
        Err(ResponsesError::ResourceLimitExceeded)
    );
}
