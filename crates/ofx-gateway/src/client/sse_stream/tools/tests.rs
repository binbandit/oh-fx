use std::collections::VecDeque;

use tokio_util::sync::CancellationToken;

use super::*;
use crate::chat_completions::ChunkSource;
use crate::client::sse_stream::{GatewayCompletion, Stream};

const TOOL_CALLS: &str = r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#;

struct Chunks(VecDeque<Vec<u8>>);

impl ChunkSource for Chunks {
    async fn next_chunk(&mut self) -> Result<Option<impl AsRef<[u8]> + Send>, String> {
        Ok(self.0.pop_front())
    }
}

fn events(lines: &[&str]) -> String {
    lines.iter().fold(String::new(), |mut wire, line| {
        wire.push_str("data: ");
        wire.push_str(line);
        wire.push_str("\n\n");
        wire
    })
}

async fn consume_with(wire: &str) -> (GatewayCompletion, Vec<StreamEvent>) {
    let mut seen = Vec::new();
    let mut sink = |event: StreamEvent| seen.push(event);
    let stream = Stream {
        requested_model: "test/model",
        secrets: &[],
    };
    let chunks = wire
        .as_bytes()
        .chunks(64 * 1024)
        .map(<[u8]>::to_vec)
        .collect();
    let completion = stream
        .consume(&mut Chunks(chunks), &mut sink, &CancellationToken::new())
        .await
        .unwrap();
    (completion, seen)
}

async fn consume(lines: &[&str]) -> GatewayCompletion {
    consume_with(&events(lines)).await.0
}

fn only(completion: &GatewayCompletion) -> &FinalCall {
    assert_eq!(completion.tools.calls.len(), 1);
    &completion.tools.calls[0]
}

fn call_event(input: &str) -> String {
    format!(r#"{{"type":"tool-call","toolCallId":"c1","toolName":"dynamic_tool","input":{input}}}"#)
}

#[tokio::test]
async fn object_and_array_inputs_are_serialized() {
    let completion = consume(&[
        r#"{"type":"tool-call","toolCallId":"c1","toolName":"ask_user_question","input":{"questions":[{"question":"ok?"}]}}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(
        only(&completion).arguments,
        r#"{"questions":[{"question":"ok?"}]}"#
    );
    let completion = consume(&[&call_event(r#"[1,{"nested":true}]"#), TOOL_CALLS]).await;
    let call = only(&completion);
    assert_eq!(call.arguments, r#"[1,{"nested":true}]"#);
    assert!(!call.malformed);
}

#[tokio::test]
async fn valid_serialized_input_keeps_its_exact_bytes_and_scalar_roots() {
    let exact = r#"  {"first":1,"second":2,"number":1e+02} "#.to_owned() + "\n";
    for input in [exact.as_str(), "null", "true", "42", "\"text\""] {
        let encoded = serde_json::to_string(input).unwrap();
        let completion = consume(&[&call_event(&encoded), TOOL_CALLS]).await;
        let call = only(&completion);
        assert_eq!(call.arguments, input);
        assert!(!call.malformed, "{input}");
    }
}

#[tokio::test]
async fn malformed_serialized_input_stays_raw_for_the_agent_to_diagnose() {
    for input in [
        "{]FX_FINAL_MALFORMED_SENTINEL",
        "{} FX_FINAL_TRAILING_SENTINEL",
        r#"{"depth":1,"depth":2}"#,
        r#"{"request":{"task":"FX_FINAL_TRUNCATED_SENTINEL"#,
    ] {
        let encoded = serde_json::to_string(input).unwrap();
        let completion = consume(&[&call_event(&encoded), TOOL_CALLS]).await;
        let call = only(&completion);
        assert_eq!(call.arguments, input);
        assert!(call.malformed);
        assert_eq!(completion.tools.admission(), Ok(()));
    }
}

#[tokio::test]
async fn an_absent_final_input_uses_the_ended_stream_with_the_same_id() {
    for final_name in ["", r#","toolName":"read_file""#] {
        let call = format!(r#"{{"type":"tool-call","toolCallId":"c1"{final_name}}}"#);
        let completion = consume(&[
            r#"{"type":"tool-input-start","id":"c1","toolName":"read_file"}"#,
            r#"{"type":"tool-input-delta","id":"c1","delta":"{\"path\":\"README.md\"}"}"#,
            r#"{"type":"tool-input-end","id":"c1"}"#,
            &call,
            TOOL_CALLS,
        ])
        .await;
        let call = only(&completion);
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, r#"{"path":"README.md"}"#);
        assert!(!call.malformed);
        assert_eq!(call.provisional_id, None);
        assert_eq!(completion.tools.admission(), Ok(()));
    }
    let duplicate_keys = consume(&[
        r#"{"type":"tool-input-start","id":"c1","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"c1","delta":" \n{\"number\":1e+02,\"duplicate\":1,\"duplicate\":2}\t"}"#,
        r#"{"type":"tool-input-end","id":"c1"}"#,
        r#"{"type":"tool-call","toolCallId":"c1"}"#,
        TOOL_CALLS,
    ])
    .await;
    assert!(only(&duplicate_keys).malformed);
}

#[tokio::test]
async fn an_unsupported_final_input_never_falls_back_to_the_stream() {
    for input in ["null", "false", "7"] {
        let call = format!(r#"{{"type":"tool-call","toolCallId":"c1","input":{input}}}"#);
        let completion = consume(&[
            r#"{"type":"tool-input-start","id":"c1","toolName":"read_file"}"#,
            r#"{"type":"tool-input-delta","id":"c1","delta":"{\"path\":\"SHOULD_NOT_SURVIVE\"}"}"#,
            r#"{"type":"tool-input-end","id":"c1"}"#,
            &call,
            TOOL_CALLS,
        ])
        .await;
        let call = only(&completion);
        assert_eq!(call.arguments, input);
        assert!(call.malformed);
    }
}

#[tokio::test]
async fn an_absent_final_input_without_an_ended_stream_is_refused() {
    let cases = [
        (vec![], ResultFailure::InvalidToolInput),
        (
            vec![
                r#"{"type":"tool-input-start","id":"c1","toolName":"read_file"}"#,
                r#"{"type":"tool-input-delta","id":"c1","delta":"{\"path\":\"partial"}"#,
            ],
            ResultFailure::IncompleteStreamedInput,
        ),
        (
            vec![
                r#"{"type":"tool-input-start","id":"other","toolName":"read_file"}"#,
                r#"{"type":"tool-input-delta","id":"other","delta":"{\"path\":\"README.md\"}"}"#,
                r#"{"type":"tool-input-end","id":"other"}"#,
            ],
            ResultFailure::InvalidToolInput,
        ),
    ];
    for (mut lines, failure) in cases {
        lines.push(r#"{"type":"tool-call","toolCallId":"c1"}"#);
        lines.push(TOOL_CALLS);
        let completion = consume(&lines).await;
        assert_eq!(completion.tools.failure, Some(failure));
        assert_eq!(
            completion.tools.admission(),
            Err("MalformedProviderResultIdentity")
        );
    }
}

#[tokio::test]
async fn final_identities_keep_their_state_and_borrow_no_stream() {
    let cases = [
        ("", Identity::Absent),
        (r#","toolCallId":"""#, Identity::Empty),
        (r#","toolCallId":7"#, Identity::WrongType),
        (r#","toolCallId":"final_1""#, Identity::Valid),
    ];
    for (field, identity) in cases {
        let call = format!(
            r#"{{"type":"tool-call","toolName":"read_file","input":{{"path":"README.md"}}{field}}}"#
        );
        let completion = consume(&[
            r#"{"type":"tool-input-start","id":"provisional_1","toolName":"read_file"}"#,
            &call,
            "[DONE]",
        ])
        .await;
        let call = only(&completion);
        assert_eq!(call.identity, identity);
        assert_eq!(call.provisional_id, None);
        let expected = if identity == Identity::Valid {
            Ok(())
        } else {
            Err("MalformedAuthoritativeToolIdentity")
        };
        assert_eq!(completion.tools.admission(), expected, "{field}");
    }
    let bare = consume(&[
        r#"{"type":"tool-input-start","id":"provisional_1","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"provisional_1","delta":"{\"path\":\"STREAMED_SENTINEL\"}"}"#,
        r#"{"type":"tool-input-end","id":"provisional_1"}"#,
        r#"{"type":"tool-call"}"#,
        "[DONE]",
    ])
    .await;
    let call = only(&bare);
    assert_eq!(
        (
            call.id.as_str(),
            call.name.as_str(),
            call.arguments.as_str()
        ),
        ("", "", "")
    );
}

#[tokio::test]
async fn a_renamed_final_call_is_reconciled_by_equivalent_input() {
    let completion = consume(&[
        r#"{"type":"reasoning-start","id":"r"}"#,
        r#"{"type":"reasoning-end","id":"r"}"#,
        r#"{"type":"tool-input-start","id":"provisional_read","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"provisional_read","delta":"{\"path\":\"README.md\"}"}"#,
        r#"{"type":"tool-input-end","id":"provisional_read"}"#,
        r#"{"type":"tool-call","toolCallId":"final_read","toolName":"read_file","input":{"path":"README.md"}}"#,
        TOOL_CALLS,
    ])
    .await;
    let call = only(&completion);
    assert_eq!(call.id, "final_read");
    assert_eq!(call.provisional_id.as_deref(), Some("provisional_read"));
    assert_eq!(completion.tools.failure, None);
    let replay = completion.replay.unwrap();
    assert!(replay.contains(r#""toolCallId":"final_read""#));
    assert!(!replay.contains("provisional_read"));
}

#[tokio::test]
async fn interleaved_renamed_calls_match_by_structure_and_differing_input_never_aliases() {
    let completion = consume(&[
        r#"{"type":"tool-input-start","id":"provisional_a","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"provisional_a","delta":"{\"path\":\"a.txt\",\"line_end\":2}"}"#,
        r#"{"type":"tool-input-end","id":"provisional_a"}"#,
        r#"{"type":"tool-input-start","id":"provisional_b","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"provisional_b","delta":"{\"path\":\"b.txt\",\"line_end\":4}"}"#,
        r#"{"type":"tool-input-end","id":"provisional_b"}"#,
        r#"{"type":"tool-call","toolCallId":"final_b","toolName":"read_file","input":{"line_end":4,"path":"b.txt"}}"#,
        r#"{"type":"tool-call","toolCallId":"final_a","toolName":"read_file","input":{"line_end":2,"path":"a.txt"}}"#,
        TOOL_CALLS,
    ])
    .await;
    let calls = &completion.tools.calls;
    assert_eq!(calls[0].provisional_id.as_deref(), Some("provisional_b"));
    assert_eq!(calls[1].provisional_id.as_deref(), Some("provisional_a"));
    let differing = consume(&[
        r#"{"type":"tool-input-start","id":"provisional_read","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"provisional_read","delta":"{\"path\":\"a.txt\"}"}"#,
        r#"{"type":"tool-input-end","id":"provisional_read"}"#,
        r#"{"type":"tool-call","toolCallId":"final_read","toolName":"read_file","input":{"path":"b.txt"}}"#,
        TOOL_CALLS,
    ])
    .await;
    let call = only(&differing);
    assert_eq!(call.provisional_id, None);
    assert_eq!(call.arguments, r#"{"path":"b.txt"}"#);
}

#[tokio::test]
async fn interleaved_streams_stay_apart_by_event_id_and_announce_each_start() {
    let (completion, seen) = consume_with(&events(&[
        r#"{"type":"tool-input-start","id":"A","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"A","delta":"{\"path\":"}"#,
        r#"{"type":"tool-input-start","id":"B","toolName":"grep_files"}"#,
        r#"{"type":"tool-input-delta","id":"B","delta":"{\"pattern\":"}"#,
        r#"{"type":"tool-input-delta","id":"A","delta":"\"alpha-A.txt\"}"}"#,
        r#"{"type":"tool-input-end","id":"A"}"#,
        r#"{"type":"tool-call","toolCallId":"A"}"#,
        r#"{"type":"tool-input-delta","id":"B","delta":"\"needle-B\"}"}"#,
        r#"{"type":"tool-input-end","id":"B"}"#,
        r#"{"type":"tool-call","toolCallId":"B"}"#,
        "[DONE]",
    ]))
    .await;
    let calls = &completion.tools.calls;
    assert_eq!(calls[0].arguments, r#"{"path":"alpha-A.txt"}"#);
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[1].arguments, r#"{"pattern":"needle-B"}"#);
    assert_eq!(calls[1].name, "grep_files");
    let starts: Vec<&str> = seen
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallStarted { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(starts, ["A", "B"]);
    let inputs = seen
        .iter()
        .filter(|event| matches!(event, StreamEvent::ToolInputDelta { .. }))
        .count();
    assert_eq!(inputs, 4);
}

#[tokio::test]
async fn conflicting_and_late_stream_events_change_nothing() {
    let (completion, seen) = consume_with(&events(&[
        r#"{"type":"tool-input-start","id":"X","toolName":"read_file"}"#,
        r#"{"type":"tool-input-start","id":"X","toolName":"read_file"}"#,
        r#"{"type":"tool-input-start","id":"X","toolName":"grep_files"}"#,
        r#"{"type":"tool-input-delta","id":"X","delta":"{\"path\":\"stable.txt\"}"}"#,
        r#"{"type":"tool-input-end","id":"X"}"#,
        r#"{"type":"tool-input-delta","id":"X","delta":"LATE_PREFIX_SENTINEL"}"#,
        r#"{"type":"tool-input-end","id":"X"}"#,
        r#"{"type":"tool-input-start","id":"X","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"missing","delta":"UNMATCHED_SENTINEL"}"#,
        r#"{"type":"tool-input-end","id":"missing"}"#,
        r#"{"type":"tool-call","toolCallId":"X"}"#,
        r#"{"type":"tool-input-delta","id":"X","delta":"FINALIZED_SENTINEL"}"#,
        r#"{"type":"tool-input-end","id":"X"}"#,
        r#"{"type":"tool-input-start","id":"X","toolName":"read_file"}"#,
        r#"{"type":"tool-input-start","toolName":"read_file"}"#,
        "[DONE]",
    ]))
    .await;
    assert_eq!(only(&completion).arguments, r#"{"path":"stable.txt"}"#);
    let starts = seen
        .iter()
        .filter(|event| matches!(event, StreamEvent::ToolCallStarted { .. }))
        .count();
    assert_eq!(starts, 1);
}

#[tokio::test]
async fn an_authoritative_final_input_wins_before_the_stream_ends() {
    let completion = consume(&[
        r#"{"type":"tool-input-start","id":"A","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"A","delta":"PARTIAL_SENTINEL"}"#,
        r#"{"type":"tool-call","toolCallId":"A","input":{"path":"authoritative.txt"}}"#,
        r#"{"type":"tool-input-delta","id":"A","delta":"LATE_SENTINEL"}"#,
        r#"{"type":"tool-input-end","id":"A"}"#,
        "[DONE]",
    ])
    .await;
    let call = only(&completion);
    assert_eq!(call.name, "read_file");
    assert_eq!(call.arguments, r#"{"path":"authoritative.txt"}"#);
    assert_eq!(completion.tools.failure, None);
}

#[tokio::test]
async fn a_final_name_that_contradicts_the_stream_is_refused() {
    for input in ["", r#","input":{"path":"authoritative.txt"}"#] {
        let call =
            format!(r#"{{"type":"tool-call","toolCallId":"A","toolName":"edit_file"{input}}}"#);
        let completion = consume(&[
            r#"{"type":"tool-input-start","id":"A","toolName":"read_file"}"#,
            r#"{"type":"tool-input-delta","id":"A","delta":"{\"path\":\"victim.txt\"}"}"#,
            r#"{"type":"tool-input-end","id":"A"}"#,
            &call,
            "[DONE]",
        ])
        .await;
        assert_eq!(
            completion.tools.failure,
            Some(ResultFailure::ConflictingToolName)
        );
        assert!(!only(&completion).arguments.contains("victim.txt"));
        assert!(completion.tools.admission().is_err());
    }
}

#[tokio::test]
async fn unmatched_finals_stay_independent_and_duplicate_ids_are_refused() {
    let unmatched = consume(&[
        r#"{"type":"tool-input-start","id":"streamed","toolName":"read_file"}"#,
        r#"{"type":"tool-call","toolCallId":"final","toolName":"grep_files","input":{"pattern":"needle"}}"#,
        "[DONE]",
    ])
    .await;
    let call = only(&unmatched);
    assert_eq!(
        (call.id.as_str(), call.name.as_str()),
        ("final", "grep_files")
    );
    assert_eq!(call.provisional_id, None);
    let duplicate = consume(&[
        r#"{"type":"tool-call","toolCallId":"duplicate","toolName":"read_file","input":{"path":"a"}}"#,
        r#"{"type":"tool-call","toolCallId":"duplicate","toolName":"read_file","input":{"path":"b"}}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(duplicate.tools.failure, None);
    assert_eq!(
        duplicate.tools.admission(),
        Err("MalformedAuthoritativeToolIdentity")
    );
    let reused = consume(&[
        r#"{"type":"tool-input-start","id":"duplicate","toolName":"read_file"}"#,
        r#"{"type":"tool-input-delta","id":"duplicate","delta":"{\"path\":\"a\"}"}"#,
        r#"{"type":"tool-input-end","id":"duplicate"}"#,
        r#"{"type":"tool-call","toolCallId":"duplicate"}"#,
        r#"{"type":"tool-call","toolCallId":"duplicate"}"#,
        "[DONE]",
    ])
    .await;
    let calls = &reused.tools.calls;
    assert_eq!(calls[0].arguments, r#"{"path":"a"}"#);
    assert_eq!(calls[1].arguments, "");
    assert_eq!(reused.tools.failure, None);
    assert_eq!(
        reused.tools.admission(),
        Err("MalformedAuthoritativeToolIdentity")
    );
}

#[tokio::test]
async fn provider_results_must_name_exactly_one_provider_executed_call() {
    let search = r#"{"type":"tool-call","toolCallId":"call_1","toolName":"parallel_search","input":{},"providerExecuted":true}"#;
    for (field, failure) in [
        ("", ResultFailure::Absent),
        (r#","toolCallId":"""#, ResultFailure::Empty),
        (r#","toolCallId":7"#, ResultFailure::WrongType),
        (r#","toolCallId":"other""#, ResultFailure::Unmatched),
    ] {
        let result = format!(r#"{{"type":"tool-result","result":{{"results":[]}}{field}}}"#);
        let completion = consume(&[search, &result, "[DONE]"]).await;
        assert_eq!(completion.tools.failure, Some(failure), "{field}");
    }
    let ambiguous = consume(&[
        r#"{"type":"tool-call","toolCallId":"duplicate_1","toolName":"parallel_search","input":{},"providerExecuted":true}"#,
        r#"{"type":"tool-call","toolCallId":"duplicate_1","toolName":"parallel_search","input":{},"providerExecuted":true}"#,
        r#"{"type":"tool-result","toolCallId":"duplicate_1","result":{"results":[]}}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(ambiguous.tools.failure, Some(ResultFailure::Ambiguous));
    assert_eq!(
        ambiguous.tools.admission(),
        Err("MalformedProviderResultIdentity")
    );
}

#[tokio::test]
async fn provider_execution_comes_from_the_final_call_with_its_final_result() {
    let completion = consume(&[
        r#"{"type":"tool-call","toolCallId":"call_1","toolName":"parallel_search","input":{},"providerExecuted":true}"#,
        r#"{"type":"tool-result","toolCallId":"call_1","preliminary":true,"result":{"first":true}}"#,
        r#"{"type":"tool-result","toolCallId":"call_1","preliminary":true,"result":{"second":true}}"#,
        r#"{"type":"tool-result","toolCallId":"call_1","result":{"results":[{"title":"ok"}]}}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(completion.tools.admission(), Ok(()));
    let calls = completion.tools.into_calls();
    assert_eq!(
        calls[0].provenance,
        ToolExecutionProvenance::ProviderExecuted
    );
    assert_eq!(
        calls[0].provider_result.as_deref(),
        Some(r#"{"results":[{"title":"ok"}]}"#)
    );
}

#[tokio::test]
async fn missing_preliminary_or_contradicted_results_are_refused() {
    let search = r#"{"type":"tool-call","toolCallId":"call_1","toolName":"parallel_search","input":{},"providerExecuted":true}"#;
    let missing = consume(&[
        search,
        r#"{"type":"tool-result","toolCallId":"call_1"}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(missing.tools.failure, Some(ResultFailure::MissingResult));
    let preliminary = consume(&[
        search,
        r#"{"type":"tool-result","toolCallId":"call_1","preliminary":true,"result":{"partial":true}}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(preliminary.tools.failure, None);
    assert_eq!(
        preliminary.tools.admission(),
        Err("MalformedProviderResultIdentity")
    );
    assert_eq!(preliminary.tools.into_calls()[0].provider_result, None);
    let flag = consume(&[
        r#"{"type":"tool-call","toolCallId":"call_1","toolName":"parallel_search","input":{},"providerExecuted":"yes"}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(only(&flag).provenance, ToolExecutionProvenance::FxLocal);
    assert_eq!(
        flag.tools.failure,
        Some(ResultFailure::MalformedProviderExecuted)
    );
    let local = consume(&[
        r#"{"type":"tool-call","toolCallId":"call_1","toolName":"read_file","input":{"path":"README.md"}}"#,
        r#"{"type":"tool-result","toolCallId":"call_1","providerExecuted":true,"result":{"ignored":true}}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(
        local.tools.failure,
        Some(ResultFailure::ProvenanceContradiction)
    );
    for (events, failure) in [
        (
            vec![
                r#"{"type":"tool-result","toolCallId":"call_1","preliminary":"yes","result":{"bad":true}}"#,
            ],
            ResultFailure::MalformedPreliminary,
        ),
        (
            vec![r#"{"type":"tool-result","toolCallId":"call_1","result":null}"#],
            ResultFailure::MissingResult,
        ),
        (
            vec![
                r#"{"type":"tool-result","toolCallId":"call_1","result":{"final":1}}"#,
                r#"{"type":"tool-result","toolCallId":"call_1","preliminary":true,"result":{"late":2}}"#,
            ],
            ResultFailure::DuplicateResult,
        ),
    ] {
        let mut lines = vec![search];
        lines.extend(events);
        lines.push("[DONE]");
        let completion = consume(&lines).await;
        assert_eq!(completion.tools.failure, Some(failure));
    }
}

#[tokio::test]
async fn provider_calls_with_malformed_input_are_refused_and_identities_must_be_storable() {
    let malformed = consume(&[
        r#"{"type":"tool-call","toolCallId":"call_1","toolName":"parallel_search","input":"{]","providerExecuted":true}"#,
        r#"{"type":"tool-result","toolCallId":"call_1","result":{}}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(
        malformed.tools.admission(),
        Err("MalformedProviderToolArguments")
    );
    let long = "n".repeat(MAX_IDENTITY_BYTES + 1);
    let call =
        format!(r#"{{"type":"tool-call","toolCallId":"call_1","toolName":"{long}","input":{{}}}}"#);
    let unstorable = consume(&[&call, "[DONE]"]).await;
    assert_eq!(
        unstorable.tools.admission(),
        Err("MalformedAuthoritativeToolIdentity")
    );
    let blank = consume(&[
        r#"{"type":"tool-call","toolCallId":" ","toolName":"read_file","input":{}}"#,
        "[DONE]",
    ])
    .await;
    assert_eq!(
        blank.tools.admission(),
        Err("MalformedAuthoritativeToolIdentity")
    );
}

#[tokio::test]
async fn a_consolidated_call_larger_than_the_transfer_buffer_arrives_whole() {
    for length in [192 * 1024, 300 * 1024, 4 * 1024 * 1024] {
        let content = "x".repeat(length);
        let call = format!(
            r#"{{"type":"tool-call","toolCallId":"large","toolName":"write_file","input":{{"path":"large.txt","content":"{content}"}}}}"#
        );
        let completion = consume(&[&call, "[DONE]"]).await;
        let parsed: serde_json::Value = serde_json::from_str(&only(&completion).arguments).unwrap();
        assert_eq!(parsed["content"].as_str().unwrap().len(), length);
    }
}
