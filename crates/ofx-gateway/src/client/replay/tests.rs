use ofx_contract::{DuplicateKeys, parse_strict_json};
use serde_json::json;

use super::*;

fn observe(
    builder: &mut ReplayBuilder,
    event: &str,
    content_offset: usize,
) -> Result<(), ReplayError> {
    let parsed = parse_strict_json(event.as_bytes(), DuplicateKeys::AfterValue).unwrap();
    let fields = parsed.as_object().unwrap();
    let kind = fields.get("type").and_then(Json::as_str).unwrap();
    builder.observe(kind, fields, content_offset)
}

fn parts(replay: &str) -> Vec<Value> {
    serde_json::from_str::<Value>(replay)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn ordered_continuation_parts_keep_their_merged_metadata() {
    let mut builder = ReplayBuilder::default();
    let mut content = 0;
    for event in [
        r#"{"type":"reasoning-start","id":"r1","providerMetadata":{"openai":{"itemId":"reason-1","reasoningEncryptedContent":"partial"}}}"#,
        r#"{"type":"reasoning-delta","id":"r1","delta":"reasoning"}"#,
        r#"{"type":"reasoning-end","id":"r1","providerMetadata":{"openai":{"reasoningEncryptedContent":"complete"}}}"#,
        r#"{"type":"text-start","id":"t1"}"#,
        r#"{"type":"text-delta","id":"t1","delta":"visible"}"#,
        r#"{"type":"text-end","id":"t1"}"#,
        r#"{"type":"tool-input-start","id":"call-1","toolName":"read_file"}"#,
        r#"{"type":"tool-call","toolCallId":"call-1","toolName":"read_file","input":{"path":"file"},"providerMetadata":{"vertex":{"thoughtSignature":"signature"}}}"#,
    ] {
        observe(&mut builder, event, content).unwrap();
        if event.contains("\"visible\"") {
            content += "visible".len();
        }
    }
    let calls = [ReplayCall {
        id: "call-1",
        provisional_id: None,
    }];
    let replay = builder.finish("visible", &calls).unwrap().unwrap();
    let parts = parts(&replay);
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0]["type"], "reasoning");
    assert_eq!(parts[0]["text"], "reasoning");
    assert_eq!(
        parts[0]["providerOptions"]["openai"],
        json!({"itemId": "reason-1", "reasoningEncryptedContent": "complete"})
    );
    assert_eq!(parts[1], json!({"type": "text", "offset": 0, "length": 7}));
    assert_eq!(parts[2]["type"], "tool-call");
    assert_eq!(parts[2]["toolCallId"], "call-1");
    assert_eq!(
        parts[2]["providerOptions"]["vertex"]["thoughtSignature"],
        "signature"
    );
}

#[test]
fn restarted_reasoning_and_text_segments_are_kept_apart() {
    let mut distinct = None;
    for reuse_ids in [false, true] {
        let mut builder = ReplayBuilder::default();
        let mut content = String::new();
        let mut ids = Vec::new();
        for label in ["A", "B", "C"] {
            let id = if reuse_ids { "0" } else { label };
            for event in [
                json!({"type": "reasoning-start", "id": id}),
                json!({"type": "reasoning-delta", "id": id, "delta": ""}),
                json!({"type": "reasoning-delta", "id": id, "delta": label}),
                json!({"type": "reasoning-end", "id": id, "providerMetadata": {"google": {"signature": label}}}),
                json!({"type": "text-start", "id": id}),
                json!({"type": "text-delta", "id": id, "delta": label}),
                json!({"type": "text-end", "id": id}),
                json!({"type": "tool-call", "toolCallId": label, "toolName": "exa_search", "input": {"query": label}, "providerExecuted": true}),
            ] {
                observe(&mut builder, &event.to_string(), content.len()).unwrap();
                if event["type"] == "text-delta" {
                    content.push_str(label);
                }
            }
            ids.push(label);
        }
        let calls: Vec<ReplayCall<'_>> = ids
            .iter()
            .map(|id| ReplayCall {
                id,
                provisional_id: None,
            })
            .collect();
        let replay = builder.finish(&content, &calls).unwrap().unwrap();
        let parts = parts(&replay);
        assert_eq!(parts.len(), 9);
        for (index, label) in ["A", "B", "C"].iter().enumerate() {
            assert_eq!(parts[index * 3]["text"], *label);
            assert_eq!(
                parts[index * 3]["providerOptions"]["google"]["signature"],
                *label
            );
            assert_eq!(parts[index * 3 + 1]["offset"], index);
            assert_eq!(parts[index * 3 + 1]["length"], 1);
            assert_eq!(parts[index * 3 + 2]["toolCallId"], *label);
        }
        match &distinct {
            Some(expected) => assert_eq!(&replay, expected),
            None => distinct = Some(replay),
        }
    }
}

#[test]
fn late_deltas_without_a_segment_restart_are_refused() {
    for kind in ["reasoning", "text"] {
        let mut builder = ReplayBuilder::default();
        observe(
            &mut builder,
            &format!(r#"{{"type":"{kind}-start","id":"0"}}"#),
            0,
        )
        .unwrap();
        observe(
            &mut builder,
            &format!(r#"{{"type":"{kind}-end","id":"0"}}"#),
            0,
        )
        .unwrap();
        assert_eq!(
            observe(
                &mut builder,
                &format!(r#"{{"type":"{kind}-delta","id":"0","delta":""}}"#),
                0
            ),
            Err(ReplayError::InvalidProviderState),
            "{kind}"
        );
    }
}

#[test]
fn reasoning_metadata_merges_across_restarts_and_an_unended_signature_is_refused() {
    let mut builder = ReplayBuilder::default();
    for event in [
        r#"{"type":"reasoning-start","id":"r","providerMetadata":{"google":{"redactedData":"opaque"}}}"#,
        r#"{"type":"reasoning-delta","id":"r","delta":"","providerMetadata":{"google":{"signature":"signed"}}}"#,
        r#"{"type":"reasoning-end","id":"r"}"#,
        r#"{"type":"reasoning-start","id":"r"}"#,
        r#"{"type":"reasoning-delta","id":"r","delta":"","providerMetadata":{"google":{"signature":"second"}}}"#,
        r#"{"type":"reasoning-end","id":"r"}"#,
    ] {
        observe(&mut builder, event, 0).unwrap();
    }
    let replay = builder.finish("", &[]).unwrap().unwrap();
    for value in ["opaque", "signed", "second"] {
        assert!(replay.contains(value), "{value}");
    }
    let mut incomplete = ReplayBuilder::default();
    observe(
        &mut incomplete,
        r#"{"type":"reasoning-start","id":"r","providerMetadata":{"google":{"signature":"partial"}}}"#,
        0,
    )
    .unwrap();
    assert_eq!(
        incomplete.finish("", &[]),
        Err(ReplayError::InvalidProviderState)
    );
    assert_eq!(
        incomplete.reserve(MAX_REPLAY_BYTES),
        Err(ReplayError::ProviderStateTooLarge)
    );
}

#[test]
fn replays_are_built_only_when_reasoning_or_provider_metadata_needs_them() {
    let mut builder = ReplayBuilder::default();
    for event in [
        r#"{"type":"text-start","id":"t","providerMetadata":{"gateway":{"routing":{}}}}"#,
        r#"{"type":"text-delta","id":"t","delta":"hi"}"#,
        r#"{"type":"tool-call","toolCallId":"c","toolName":"read_file","input":{}}"#,
    ] {
        observe(&mut builder, event, 0).unwrap();
    }
    assert_eq!(builder.finish("hi", &[]), Ok(None));
}

#[test]
fn provider_metadata_must_be_objects_and_name_a_part() {
    for event in [
        r#"{"type":"text-start","id":"t","providerMetadata":[]}"#,
        r#"{"type":"text-start","id":"t","providerMetadata":{"openai":"flat"}}"#,
        r#"{"type":"text-start","providerMetadata":{"openai":{"itemId":"x"}}}"#,
        r#"{"type":"text-delta","id":"t","delta":7}"#,
    ] {
        let mut builder = ReplayBuilder::default();
        assert_eq!(
            observe(&mut builder, event, 0),
            Err(ReplayError::InvalidProviderState),
            "{event}"
        );
    }
    let long = "i".repeat(MAX_IDENTITY_BYTES + 1);
    let mut builder = ReplayBuilder::default();
    assert_eq!(
        observe(
            &mut builder,
            &format!(r#"{{"type":"text-start","id":"{long}"}}"#),
            0
        ),
        Err(ReplayError::ProviderStateTooLarge)
    );
    assert_eq!(
        observe(
            &mut builder,
            &format!(r#"{{"type":"tool-call","toolCallId":"{long}"}}"#),
            0
        ),
        Ok(())
    );
}

#[test]
fn tool_call_parts_follow_their_canonical_call_and_merge_duplicates() {
    let mut builder = ReplayBuilder::default();
    for event in [
        r#"{"type":"reasoning-start","id":"r"}"#,
        r#"{"type":"reasoning-end","id":"r"}"#,
        r#"{"type":"tool-input-start","id":"provisional","toolName":"read_file","providerMetadata":{"vertex":{"first":1}}}"#,
        r#"{"type":"tool-call","toolCallId":"final","toolName":"read_file","providerMetadata":{"vertex":{"second":2}}}"#,
        r#"{"type":"tool-input-start","id":"orphan","toolName":"read_file"}"#,
    ] {
        observe(&mut builder, event, 0).unwrap();
    }
    let calls = [ReplayCall {
        id: "final",
        provisional_id: Some("provisional"),
    }];
    let parts = parts(&builder.finish("", &calls).unwrap().unwrap());
    assert_eq!(parts.len(), 2);
    assert_eq!(
        parts[1],
        json!({"type": "tool-call", "toolCallId": "final", "providerOptions": {"vertex": {"first": 1, "second": 2}}})
    );
    let mut unmatched = ReplayBuilder::default();
    observe(
        &mut unmatched,
        r#"{"type":"tool-call","toolCallId":"lost","providerMetadata":{"vertex":{"x":1}}}"#,
        0,
    )
    .unwrap();
    assert_eq!(
        unmatched.finish("", &[]),
        Err(ReplayError::InvalidProviderState)
    );
}
