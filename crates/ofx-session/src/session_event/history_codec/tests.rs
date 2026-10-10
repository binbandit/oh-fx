use ofx_config::ProviderId;
use ofx_contract::{CommandProcessPresentation, TurnSummary, TurnTokenProgress};

use super::*;
use crate::session_event::InterruptReason;

fn envelope(seq: u64, timestamp_ms: i64, event: ConversationEvent) -> ConversationEnvelope {
    ConversationEnvelope {
        schema_version: CONVERSATION_SCHEMA_VERSION,
        seq,
        timestamp_ms,
        event,
    }
}

fn encoded(envelope: &ConversationEnvelope) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode_history_envelope(&mut bytes, envelope).unwrap();
    bytes
}

fn steering(text: &str) -> ConversationEvent {
    ConversationEvent::Steering(SteeringEvent {
        text: text.to_owned(),
    })
}

fn word(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

fn round_trip(envelope: &ConversationEnvelope) {
    let bytes = encoded(envelope);
    let decoded = decode_history_envelope(&bytes).unwrap();
    assert_eq!(decoded.seq, envelope.seq);
    assert_eq!(decoded.timestamp_ms, envelope.timestamp_ms);
    assert_eq!(decoded.event, envelope.event);
    assert_eq!(encoded(&decoded), bytes);
}

fn file_evidence() -> FileEvidence {
    FileEvidence {
        path: "src/a.zig".to_owned(),
        new_path: Some("src/b.zig".to_owned()),
        tool_call_id: "c1".to_owned(),
        tool_name: "edit".to_owned(),
        action: FileEvidenceAction::Edit,
        status: ToolResultStatus::Success,
        model_view_covers_full_file: true,
        stale: false,
    }
}

#[test]
fn history_snapshot_codec_round_trips_every_event_kind() {
    round_trip(&envelope(
        1,
        1,
        ConversationEvent::User(UserEvent::for_work("hello world", "work-1")),
    ));
    round_trip(&envelope(
        2,
        2,
        ConversationEvent::User(UserEvent::new("plain")),
    ));
    round_trip(&envelope(
        3,
        3,
        ConversationEvent::Assistant(AssistantEvent {
            text: "answer".to_owned(),
            provider_replay: Some(SavedReplay {
                source: SavedReplaySource {
                    provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
                    model: "kimi-k3".to_owned(),
                },
                parts_json: "[{\"type\":\"text\"}]".to_owned(),
            }),
            standalone_response: true,
        }),
    ));
    round_trip(&envelope(
        4,
        4,
        ConversationEvent::Assistant(AssistantEvent {
            text: String::new(),
            provider_replay: Some(SavedReplay {
                source: SavedReplaySource {
                    provider: SavedProvider::new(
                        ProviderId::Configured("router".to_owned()),
                        Some([7; 32]),
                    )
                    .unwrap(),
                    model: "local".to_owned(),
                },
                parts_json: "[]".to_owned(),
            }),
            standalone_response: false,
        }),
    ));
    let mut call = ToolCallEvent::new(
        "c1",
        "shell",
        "{\"command\":\"ls\"}",
        ToolArgumentIntegrity::MalformedJson,
    );
    call.provider_result = Some("{\"ok\":true}".to_owned());
    call.provenance = ToolExecutionProvenance::ProviderExecuted;
    round_trip(&envelope(5, 5, ConversationEvent::ToolCall(call)));
    let mut result = ToolResultEvent::new(
        "c1",
        "shell",
        ToolResultStatus::Failure,
        "tool-results/abc",
        512,
        ArtifactCompleteness::Partial,
    );
    result.output_bytes = Some(1024);
    result.preview = Some("first bytes".to_owned());
    result.provider_native = true;
    result.created_at_ms = 42;
    result.permission_feedback = vec!["run the tests".to_owned(), "skip the docs".to_owned()];
    round_trip(&envelope(6, 6, ConversationEvent::ToolResult(result)));
    round_trip(&envelope(7, 7, steering("keep going")));
    round_trip(&envelope(
        8,
        8,
        ConversationEvent::TurnCompleted(TurnCompletedEvent {
            files: vec![file_evidence()],
            turn_summary: None,
        }),
    ));
    let mut interrupted =
        InterruptedEvent::new(InterruptReason::Cancelled, Some("partial".to_owned()));
    interrupted.files = vec![file_evidence()];
    round_trip(&envelope(9, 9, ConversationEvent::Interrupted(interrupted)));
    round_trip(&envelope(
        10,
        10,
        ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
            covers_through_seq: 7,
            summary: "summary text".to_owned(),
        }),
    ));
}

#[test]
fn history_snapshot_codec_round_trips_command_outcomes() {
    let mut result = ToolResultEvent::new(
        "c1",
        "shell",
        ToolResultStatus::Failure,
        "tool-results/abc",
        512,
        ArtifactCompleteness::Complete,
    );
    for presentation in [
        CommandProcessPresentation::ExitCode(-1),
        CommandProcessPresentation::Signal(9),
        CommandProcessPresentation::TimedOut,
        CommandProcessPresentation::OutputCaptureFailed,
    ] {
        result.command_process_presentation = Some(presentation);
        round_trip(&envelope(
            6,
            6,
            ConversationEvent::ToolResult(result.clone()),
        ));
    }
}

#[test]
fn history_snapshot_codec_writes_upstream_envelope_and_user_layout() {
    let mut expected = Vec::new();
    expected.extend(word(3));
    expected.extend(word(9));
    expected.extend(word(1_726_000_000_000));
    expected.push(4);
    expected.extend(word(10));
    expected.extend(b"keep going");
    assert_eq!(
        encoded(&envelope(9, 1_726_000_000_000, steering("keep going"))),
        expected
    );

    let mut expected = Vec::new();
    expected.extend(word(3));
    expected.extend(word(1));
    expected.extend(word(1));
    expected.push(0);
    expected.extend(word(10));
    expected.extend(b"with image");
    expected.extend(word(0));
    expected.push(0);
    assert_eq!(
        encoded(&envelope(
            1,
            1,
            ConversationEvent::User(UserEvent::new("with image"))
        )),
        expected
    );
}

#[test]
fn history_snapshot_codec_writes_upstream_tool_result_layout() {
    let result = ToolResultEvent::new(
        "c1",
        "shell",
        ToolResultStatus::Success,
        "a",
        2,
        ArtifactCompleteness::Unknown,
    );
    let mut expected = Vec::new();
    expected.extend(word(3));
    expected.extend(word(2));
    expected.extend(word(5));
    expected.push(3);
    expected.extend(word(2));
    expected.extend(b"c1");
    expected.extend(word(5));
    expected.extend(b"shell");
    expected.push(0);
    expected.extend(word(1));
    expected.extend(b"a");
    expected.push(0);
    expected.push(0);
    expected.extend(word(2));
    expected.push(2);
    expected.extend([0, 0, 0]);
    expected.extend(word(0));
    expected.extend(word(0));
    let mut signalled = expected.clone();
    expected.extend([0, 0, 0, 0, 0]);
    assert_eq!(
        encoded(&envelope(
            2,
            5,
            ConversationEvent::ToolResult(result.clone())
        )),
        expected
    );

    signalled.extend([0, 0, 0, 1, 1]);
    signalled.extend(word(9));
    signalled.push(0);
    let mut result = result;
    result.command_process_presentation = Some(CommandProcessPresentation::Signal(9));
    let bytes = encoded(&envelope(2, 5, ConversationEvent::ToolResult(result)));
    assert_eq!(bytes, signalled);
    let tag = bytes.len() - 10;
    for (at, value) in [(tag, 4), (tag + 5, 1)] {
        let mut corrupt = bytes.clone();
        corrupt[at] = value;
        assert!(decode_history_envelope(&corrupt).is_none(), "byte {at}");
    }
}

#[test]
fn history_snapshot_codec_writes_upstream_interrupted_layout() {
    let mut expected = Vec::new();
    expected.extend(word(3));
    expected.extend(word(4));
    expected.extend(word(6));
    expected.push(6);
    expected.push(1);
    expected.extend([0, 0, 0, 0]);
    expected.extend(word(0));
    expected.extend([0, 0]);
    let interrupted = InterruptedEvent::new(InterruptReason::Failed, None);
    assert_eq!(
        encoded(&envelope(4, 6, ConversationEvent::Interrupted(interrupted))),
        expected
    );
}

#[test]
fn history_snapshot_codec_writes_turn_summaries_in_upstream_layout() {
    let summary = TurnSummary {
        started_at_ms: 1000,
        completed_at_ms: 4500,
        thinking_duration_ms: 1200,
        turn_duration_ms: 3500,
        token_progress: TurnTokenProgress {
            input_tokens: 1234,
            output_tokens: 340,
            input_exact: true,
            output_exact: false,
        },
    };
    let mut summary_bytes = vec![1];
    for value in [1000, 4500, 1200, 3500, 1234, 340] {
        summary_bytes.extend(word(value));
    }
    summary_bytes.extend([1, 0]);
    let mut expected = Vec::new();
    expected.extend(word(3));
    expected.extend(word(4));
    expected.extend(word(6));
    expected.push(5);
    expected.extend(word(0));
    expected.extend(&summary_bytes);
    let completed = envelope(
        4,
        6,
        ConversationEvent::TurnCompleted(TurnCompletedEvent {
            files: Vec::new(),
            turn_summary: Some(summary),
        }),
    );
    assert_eq!(encoded(&completed), expected);
    round_trip(&completed);
    let mut interrupted = InterruptedEvent::new(InterruptReason::Cancelled, None);
    interrupted.turn_summary = Some(summary);
    let interrupted = envelope(4, 6, ConversationEvent::Interrupted(interrupted));
    let mut expected = Vec::new();
    expected.extend(word(3));
    expected.extend(word(4));
    expected.extend(word(6));
    expected.extend([6, 0, 0, 0, 0, 0]);
    expected.extend(word(0));
    expected.extend(&summary_bytes);
    expected.push(0);
    assert_eq!(encoded(&interrupted), expected);
    round_trip(&interrupted);
    let mut torn = encoded(&completed);
    torn.truncate(torn.len() - 1);
    assert!(decode_history_envelope(&torn).is_none());
    let mut bad_flag = encoded(&completed);
    let last = bad_flag.len() - 1;
    bad_flag[last] = 2;
    assert!(decode_history_envelope(&bad_flag).is_none());
}

#[test]
fn history_snapshot_codec_writes_upstream_provider_layout() {
    let provider =
        SavedProvider::new(ProviderId::Configured("router".to_owned()), Some([7; 32])).unwrap();
    let assistant = AssistantEvent {
        text: "a".to_owned(),
        provider_replay: Some(SavedReplay {
            source: SavedReplaySource {
                provider,
                model: "m".to_owned(),
            },
            parts_json: "[]".to_owned(),
        }),
        standalone_response: false,
    };
    let mut name = [0_u8; 64];
    name[..6].copy_from_slice(b"router");
    let mut expected = Vec::new();
    expected.extend(word(3));
    expected.extend(word(5));
    expected.extend(word(7));
    expected.push(1);
    expected.extend(word(1));
    expected.extend(b"a");
    expected.push(1);
    expected.push(3);
    expected.extend(name);
    expected.extend(word(6));
    expected.push(1);
    expected.extend([7; 32]);
    expected.extend(word(1));
    expected.extend(b"m");
    expected.extend(word(2));
    expected.extend(b"[]");
    expected.push(0);
    assert_eq!(
        encoded(&envelope(5, 7, ConversationEvent::Assistant(assistant))),
        expected
    );
}

#[test]
fn history_snapshot_codec_rejects_truncated_and_corrupt_payloads() {
    let bytes = encoded(&envelope(1, 1, steering("keep going")));
    assert!(decode_history_envelope(&bytes[..bytes.len() - 3]).is_none());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(decode_history_envelope(&trailing).is_none());
    assert_eq!(decode_history_envelope(&bytes).unwrap().seq, 1);

    let mut unknown_event = bytes.clone();
    unknown_event[24] = 8;
    assert!(decode_history_envelope(&unknown_event).is_none());
    let mut old_schema = bytes.clone();
    old_schema[0] = 2;
    assert!(decode_history_envelope(&old_schema).is_none());
    let mut no_seq = bytes.clone();
    no_seq[8] = 0;
    assert!(decode_history_envelope(&no_seq).is_none());
    let mut negative_time = bytes.clone();
    negative_time[23] = 0x80;
    assert!(decode_history_envelope(&negative_time).is_none());
    let mut invalid_text = bytes.clone();
    invalid_text[33] = 0xff;
    assert!(decode_history_envelope(&invalid_text).is_none());
    let mut huge_text = bytes;
    huge_text[29] = 1;
    assert!(decode_history_envelope(&huge_text).is_none());
    let empty = encoded(&envelope(1, 1, steering("x")));
    let mut empty_text = empty[..25].to_vec();
    empty_text.extend(word(0));
    assert!(decode_history_envelope(&empty_text).is_none());

    let user = encoded(&envelope(
        1,
        1,
        ConversationEvent::User(UserEvent::new("hi")),
    ));
    let mut with_images = user.clone();
    with_images[35] = 1;
    assert!(decode_history_envelope(&with_images).is_none());
    let mut bad_option = user;
    let last = bad_option.len() - 1;
    bad_option[last] = 2;
    assert!(decode_history_envelope(&bad_option).is_none());

    let result = ToolResultEvent::new(
        "c1",
        "shell",
        ToolResultStatus::Success,
        "a",
        2,
        ArtifactCompleteness::Unknown,
    );
    let plain = encoded(&envelope(2, 5, ConversationEvent::ToolResult(result)));
    let image_handle = 25 + 10 + 13 + 1 + 9;
    for fixed in [
        image_handle,
        image_handle + 13,
        image_handle + 22,
        plain.len() - 1,
    ] {
        let mut set = plain.clone();
        set[fixed] = 1;
        assert!(decode_history_envelope(&set).is_none(), "byte {fixed}");
    }
    let mut bad_status = plain;
    bad_status[25 + 10 + 13] = 2;
    assert!(decode_history_envelope(&bad_status).is_none());

    let interrupted = encoded(&envelope(
        4,
        6,
        ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None)),
    ));
    let mut unknown_origin = interrupted;
    let last = unknown_origin.len() - 1;
    unknown_origin[last] = 2;
    assert!(decode_history_envelope(&unknown_origin).is_none());
}

fn text_bytes(text: &str) -> Vec<u8> {
    let mut bytes = word(u64::try_from(text.len()).unwrap()).to_vec();
    bytes.extend(text.as_bytes());
    bytes
}

#[test]
fn history_snapshot_codec_keeps_command_replays_in_upstream_layout() {
    let handle = "fx-command-replay-1.bin";
    let mut result = ToolResultEvent::new(
        "c1",
        "shell",
        ToolResultStatus::Success,
        handle,
        2,
        ArtifactCompleteness::Complete,
    );
    result.command_replay_ref = Some(handle.to_owned());
    result.command_replay_bytes = Some(29);
    let bytes = encoded(&envelope(
        2,
        5,
        ConversationEvent::ToolResult(result.clone()),
    ));
    let mut tail = vec![0, 1];
    tail.extend(text_bytes(handle));
    tail.push(1);
    tail.extend(word(29));
    tail.extend([0, 0]);
    assert!(bytes.ends_with(&tail), "{bytes:?}");
    round_trip(&envelope(
        2,
        5,
        ConversationEvent::ToolResult(result.clone()),
    ));

    let mut interrupted = InterruptedEvent::new(InterruptReason::Cancelled, None);
    interrupted.command_replay_ref = Some(handle.to_owned());
    interrupted.command_replay_bytes = Some(29);
    interrupted.command_artifact_ref = Some("fx-command-artifact-1.log".to_owned());
    let mut expected = Vec::new();
    expected.extend(word(3));
    expected.extend(word(4));
    expected.extend(word(6));
    expected.extend([6, 0, 0, 1]);
    expected.extend(text_bytes(handle));
    expected.push(1);
    expected.extend(word(29));
    expected.push(1);
    expected.extend(text_bytes("fx-command-artifact-1.log"));
    expected.extend(word(0));
    expected.extend([0, 0]);
    let event = ConversationEvent::Interrupted(interrupted.clone());
    assert_eq!(encoded(&envelope(4, 6, event.clone())), expected);
    round_trip(&envelope(4, 6, event));

    result.command_replay_bytes = None;
    let unpaired = encoded(&envelope(2, 5, ConversationEvent::ToolResult(result)));
    assert!(decode_history_envelope(&unpaired).is_none());
    interrupted.command_replay_ref = None;
    let unpaired = encoded(&envelope(4, 6, ConversationEvent::Interrupted(interrupted)));
    assert!(decode_history_envelope(&unpaired).is_none());
}

#[test]
fn history_snapshot_codec_keeps_file_presentations_in_upstream_layout() {
    use crate::session_event::file_presentation::{
        CommittedFilePresentation, LifecycleId, LineKind, PresentationKind, PresentationLine,
    };

    let mut result = ToolResultEvent::new(
        "c1",
        "edit_file",
        ToolResultStatus::Success,
        "a",
        2,
        ArtifactCompleteness::Complete,
    );
    result.committed_file_presentation = Some(Box::new(CommittedFilePresentation {
        path: "a".to_owned(),
        kind: PresentationKind::Edited,
        lines: vec![PresentationLine {
            kind: LineKind::Addition,
            old_line: None,
            new_line: Some(2),
            text: "x".to_owned(),
        }],
        additions: 1,
        deletions: 0,
        truncated: false,
        previous_content: None,
        after_content: Some("y".to_owned()),
        lifecycle_id: Some(LifecycleId {
            turn_id: 4,
            call_id: "c".to_owned(),
        }),
        content_handle: None,
    }));
    let mut presentation = vec![1];
    presentation.extend(text_bytes("a"));
    presentation.push(1);
    presentation.extend(word(1));
    presentation.extend([1, 0, 1]);
    presentation.extend(word(2));
    presentation.extend(text_bytes("x"));
    presentation.extend(word(1));
    presentation.extend(word(0));
    presentation.extend([0, 0, 1]);
    presentation.extend(text_bytes("y"));
    presentation.push(1);
    presentation.extend(word(4));
    presentation.extend(text_bytes("c"));
    presentation.extend([0, 0, 0, 0, 0]);
    let bytes = encoded(&envelope(
        2,
        5,
        ConversationEvent::ToolResult(result.clone()),
    ));
    assert!(bytes.ends_with(&presentation), "{bytes:?}");
    round_trip(&envelope(
        2,
        5,
        ConversationEvent::ToolResult(result.clone()),
    ));

    let start = bytes.len() - presentation.len();
    let kind = start + 1 + 9;
    let line_kind = kind + 1 + 8;
    for (at, value) in [(kind, 2), (line_kind, 5), (line_kind + 1, 2)] {
        let mut corrupt = bytes.clone();
        corrupt[at] = value;
        assert!(decode_history_envelope(&corrupt).is_none(), "byte {at}");
    }
    let mut wide_line = bytes.clone();
    wide_line[line_kind + 3 + 4] = 1;
    assert!(decode_history_envelope(&wide_line).is_none());

    if let Some(presentation) = result.committed_file_presentation.as_mut() {
        presentation.content_handle = Some("diff-1.json".to_owned());
    }
    let competing = encoded(&envelope(2, 5, ConversationEvent::ToolResult(result)));
    assert!(decode_history_envelope(&competing).is_none());
}

#[test]
fn history_snapshot_codec_keeps_review_feedback_in_upstream_layout() {
    let mut result = ToolResultEvent::new(
        "c1",
        "shell",
        ToolResultStatus::Failure,
        "a",
        2,
        ArtifactCompleteness::Unknown,
    );
    result.review_feedback = true;
    let event = ConversationEvent::ToolResult(result);
    let bytes = encoded(&envelope(2, 5, event.clone()));
    let mut tail = vec![2, 0, 0, 1];
    tail.extend(word(0));
    tail.extend(word(0));
    tail.extend([0, 0, 0, 0, 0]);
    assert!(bytes.ends_with(&tail), "{bytes:?}");
    round_trip(&envelope(2, 5, event));

    let status = 48;
    assert_eq!(bytes[status], 1);
    let review = bytes.len() - tail.len() + 3;
    for (at, value) in [(review, 2), (review - 1, 1), (status, 0)] {
        let mut corrupt = bytes.clone();
        corrupt[at] = value;
        assert!(decode_history_envelope(&corrupt).is_none(), "byte {at}");
    }
}
