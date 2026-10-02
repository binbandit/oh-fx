use ofx_config::ProviderId;
use ofx_contract::ToolResultStatus;
use serde_json::{Map, Value};

use super::*;
use crate::json_fields::parse_json;
use crate::session_codec::SavedProvider;
use crate::session_event::{ArtifactCompleteness, InterruptReason, encode_conversation_frame};

type Decoded = (u8, u64, i64, ConversationEvent);

fn serde_decoded(bytes: &[u8]) -> Option<Decoded> {
    let envelope: ConversationEnvelope = serde_json::from_slice(bytes).ok()?;
    Some((
        envelope.schema_version,
        envelope.seq,
        envelope.timestamp_ms,
        envelope.event,
    ))
}

fn hand_decoded(bytes: &[u8]) -> Option<Decoded> {
    let envelope = parse_json(bytes).ok().and_then(envelope_from)?;
    Some((
        envelope.schema_version,
        envelope.seq,
        envelope.timestamp_ms,
        envelope.event,
    ))
}

fn replay(provider: SavedProvider) -> SavedReplay {
    SavedReplay {
        source: SavedReplaySource {
            provider,
            model: "model".to_owned(),
        },
        parts_json: "[{\"type\":\"reasoning\"}]".to_owned(),
    }
}

fn every_event() -> Vec<ConversationEvent> {
    let configured =
        SavedProvider::new(ProviderId::Configured("router".to_owned()), Some([7; 32])).unwrap();
    let gateway = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    let mut result = ToolResultEvent::new(
        "call-1",
        "shell",
        ToolResultStatus::Failure,
        "result-shell.txt",
        12,
        ArtifactCompleteness::Partial,
    );
    result.output_bytes = Some(40);
    result.preview = Some("head".to_owned());
    result.created_at_ms = 9;
    vec![
        ConversationEvent::User(UserEvent::new("hello")),
        ConversationEvent::Assistant(AssistantEvent {
            text: "text".to_owned(),
            provider_replay: Some(replay(configured)),
            standalone_response: true,
        }),
        ConversationEvent::Assistant(AssistantEvent {
            text: String::new(),
            provider_replay: Some(replay(gateway)),
            standalone_response: false,
        }),
        ConversationEvent::ToolCall(ToolCallEvent::new(
            "call-1",
            "shell",
            "{\"command\":\"ls\"}",
            ToolArgumentIntegrity::MalformedJson,
        )),
        ConversationEvent::ToolResult(result),
        ConversationEvent::Steering(SteeringEvent {
            text: "steer".to_owned(),
        }),
        ConversationEvent::TurnCompleted(TurnCompletedEvent::default()),
        ConversationEvent::Interrupted(InterruptedEvent::new(
            InterruptReason::Failed,
            Some("partial".to_owned()),
        )),
        ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
            covers_through_seq: 4,
            summary: "summary".to_owned(),
        }),
    ]
}

fn canonical(event: &ConversationEvent) -> Value {
    let frame = encode_conversation_frame(5, 6, event).unwrap();
    let mut document: Value = serde_json::from_slice(&frame).unwrap();
    let body = document["event"]
        .as_object_mut()
        .and_then(|tagged| tagged.values_mut().next())
        .and_then(Value::as_object_mut)
        .unwrap();
    match event {
        ConversationEvent::ToolResult(_) => {
            body.insert("review_feedback".to_owned(), Value::Bool(false));
        }
        ConversationEvent::Interrupted(_) => {
            body.insert("cancellation_origin".to_owned(), Value::from("turn"));
        }
        _ => {}
    }
    document
}

fn samples() -> Vec<Value> {
    [
        "null",
        "true",
        "false",
        "0",
        "-0",
        "1",
        "3",
        "-1",
        "255",
        "256",
        "1.5",
        "3.0",
        "1e2",
        "9223372036854775807",
        "9223372036854775808",
        "18446744073709551615",
        "18446744073709551616",
        "-9223372036854775808",
        "-9223372036854775809",
        "\"\"",
        "\"x\"",
        "\"valid\"",
        "\"fx_local\"",
        "\"turn\"",
        "\"success\"",
        "\"failure\"",
        "\"complete\"",
        "\"partial\"",
        "\"unknown\"",
        "\"cancelled\"",
        "\"failed\"",
        "\"malformed_json\"",
        "\"non_object_json\"",
        "\"gateway\"",
        "\"router\"",
        "[]",
        "[1]",
        "[\"x\"]",
        "{}",
        "{\"a\":1}",
        "{\"user\":{\"text\":\"x\"}}",
        "{\"name\":\"router\",\"binding\":\"0707070707070707070707070707070707070707070707070707070707070707\"}",
    ]
    .iter()
    .map(|text| serde_json::from_str(text).unwrap())
    .collect()
}

fn key_paths(value: &Value, prefix: &mut Vec<String>, paths: &mut Vec<Vec<String>>) {
    let Value::Object(fields) = value else {
        return;
    };
    for (key, child) in fields {
        prefix.push(key.clone());
        paths.push(prefix.clone());
        key_paths(child, prefix, paths);
        prefix.pop();
    }
}

fn at_mut<'a>(value: &'a mut Value, path: &[String]) -> &'a mut Map<String, Value> {
    let (_, parent) = path.split_last().unwrap();
    let mut object = value;
    for key in parent {
        object = &mut object[key.as_str()];
    }
    object.as_object_mut().unwrap()
}

fn render(value: &Value, duplicate: Option<(&[String], &Value)>) -> String {
    let Value::Object(fields) = value else {
        return value.to_string();
    };
    let mut entries: Vec<String> = fields
        .iter()
        .map(|(key, child)| {
            let nested = duplicate
                .filter(|(path, _)| path.len() > 1 && path[0] == *key)
                .map(|(path, extra)| (&path[1..], extra));
            format!("{}:{}", Value::from(key.as_str()), render(child, nested))
        })
        .collect();
    if let Some(([key], extra)) = duplicate {
        entries.push(format!("{}:{extra}", Value::from(key.as_str())));
    }
    format!("{{{}}}", entries.join(","))
}

fn assert_same(text: &str, stricter: bool) -> usize {
    let frame = format!("{text}\n");
    let serde = serde_decoded(frame.as_bytes());
    let hand = hand_decoded(frame.as_bytes());
    if serde.is_some() && hand.is_none() && stricter {
        return 1;
    }
    assert_eq!(hand, serde, "{text}");
    0
}

fn seq_form_reaches(value: &Value, path: &[String]) -> bool {
    let mut current = value;
    for key in path {
        current = &current[key.as_str()];
    }
    current.is_object()
}

#[test]
fn the_hand_decoder_accepts_exactly_what_the_serde_decoder_accepts() {
    let samples = samples();
    let mut stricter = 0;
    let mut compared = 0;
    for event in every_event() {
        let document = canonical(&event);
        let text = render(&document, None);
        assert_eq!(
            hand_decoded(format!("{text}\n").as_bytes()).unwrap().3,
            event
        );
        assert_same(&text, false);
        let mut paths = Vec::new();
        key_paths(&document, &mut Vec::new(), &mut paths);
        for path in &paths {
            let replaced_struct = seq_form_reaches(&document, path);
            let mut removed = document.clone();
            at_mut(&mut removed, path).shift_remove(path.last().unwrap());
            stricter += assert_same(&render(&removed, None), false);
            for sample in &samples {
                let mut replaced = document.clone();
                at_mut(&mut replaced, path).insert(path.last().unwrap().clone(), sample.clone());
                stricter += assert_same(
                    &render(&replaced, None),
                    replaced_struct && sample.is_array(),
                );
                stricter += assert_same(&render(&document, Some((path, sample))), false);
                compared += 2;
            }
            let mut extended = document.clone();
            at_mut(&mut extended, path).insert("unexpected".to_owned(), Value::Null);
            stricter += assert_same(&render(&extended, None), false);
            compared += 2;
        }
    }
    for sample in &samples {
        assert_same(&sample.to_string(), sample.is_array());
    }
    assert!(compared > 8_000, "{compared}");
    assert!(stricter > 0);
}

#[test]
fn the_hand_decoder_is_stricter_only_on_shapes_the_writer_never_writes() {
    let document = canonical(&ConversationEvent::Steering(SteeringEvent {
        text: "x".to_owned(),
    }));
    let accepted_by_serde_only = [
        "[3,1,1,{\"steering\":{\"text\":\"x\"}}]".to_owned(),
        render(&document, None).replace("{\"text\":\"x\"}", "[\"x\"]"),
        "{\"seq\":1,\"timestamp_ms\":1,\"event\":{\"turn_completed\":[]}}".to_owned(),
        "{\"seq\":1,\"timestamp_ms\":1,\"event\":{\"interrupted\":{\"reason\":{\"failed\":null}}}}"
            .to_owned(),
    ];
    for text in accepted_by_serde_only {
        let frame = format!("{text}\n");
        assert!(serde_decoded(frame.as_bytes()).is_some(), "{text}");
        assert!(hand_decoded(frame.as_bytes()).is_none(), "{text}");
    }
}

#[test]
fn wire_tags_match_the_serialized_enum_names() {
    for completeness in ArtifactCompleteness::ALL {
        assert_eq!(
            serde_json::to_value(completeness).unwrap(),
            completeness.tag()
        );
    }
    for reason in InterruptReason::ALL {
        assert_eq!(serde_json::to_value(reason).unwrap(), reason.tag());
    }
}
