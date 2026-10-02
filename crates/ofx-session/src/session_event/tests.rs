use ofx_config::ProviderId;

use super::*;

fn encode(seq: u64, event: &ConversationEvent) -> String {
    String::from_utf8(encode_conversation_frame(seq, 1, event).unwrap()).unwrap()
}

fn decode(frame: &str) -> Result<ConversationEvent, SessionError> {
    decode_conversation_frame(frame.as_bytes()).map(|envelope| envelope.event)
}

fn user(text: &str) -> ConversationEvent {
    ConversationEvent::User(UserEvent::new(text))
}

fn assistant(text: &str) -> ConversationEvent {
    ConversationEvent::Assistant(AssistantEvent {
        text: text.to_owned(),
        provider_replay: None,
        standalone_response: false,
    })
}

fn call(id: &str, name: &str) -> ConversationEvent {
    ConversationEvent::ToolCall(ToolCallEvent::new(
        id,
        name,
        "{}",
        ToolArgumentIntegrity::Valid,
    ))
}

fn result(id: &str, name: &str) -> ConversationEvent {
    ConversationEvent::ToolResult(ToolResultEvent::new(
        id,
        name,
        ToolResultStatus::Success,
        "result.txt",
        4,
        ArtifactCompleteness::Complete,
    ))
}

fn checkpoint(covers_through_seq: u64) -> ConversationEvent {
    ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
        covers_through_seq,
        summary: "summary".to_owned(),
    })
}

fn completed() -> ConversationEvent {
    ConversationEvent::TurnCompleted(TurnCompletedEvent::default())
}

fn state_after(events: &[ConversationEvent]) -> ConversationState {
    let mut state = ConversationState::default();
    for event in events {
        let seq = state.next_seq().unwrap();
        state.apply(seq, 10, event).unwrap();
    }
    state
}

fn apply(state: &ConversationState, event: &ConversationEvent) -> Result<(), SessionError> {
    state.clone().apply(state.last_seq() + 1, 10, event)
}

#[test]
fn conversation_cancellation_provenance_preserves_ordinary_frame_bytes() {
    let event =
        ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Cancelled, None));
    assert_eq!(
        encode(1, &event),
        "{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":1,\"event\":{\"interrupted\":{\"reason\":\"cancelled\",\"partial_text\":null,\"command_replay_ref\":null,\"command_replay_bytes\":null,\"command_artifact_ref\":null,\"files\":[],\"turn_summary\":null}}}\n"
    );
    for reason in ["cancelled", "failed"] {
        let frame = format!(
            "{{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":1,\"event\":{{\"interrupted\":{{\"reason\":\"{reason}\"}}}}}}\n"
        );
        assert!(matches!(
            decode(&frame).unwrap(),
            ConversationEvent::Interrupted(_)
        ));
        let explicit = frame.replace("\"}}}", "\",\"cancellation_origin\":\"turn\"}}}");
        assert!(decode(&explicit).is_ok(), "{explicit}");
    }
}

#[test]
fn every_event_kind_is_written_in_upstream_shape_and_round_trips() {
    let mut replayed = AssistantEvent {
        text: String::new(),
        provider_replay: Some(SavedReplay {
            source: SavedReplaySource {
                provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
                model: "test".to_owned(),
            },
            parts_json: "[{\"type\":\"reasoning\",\"text\":\"\"}]".to_owned(),
        }),
        standalone_response: true,
    };
    let mut tool_result = ToolResultEvent::new(
        "call-1",
        "shell",
        ToolResultStatus::Failure,
        "result-shell-0011223344556677-8899aabbccddeeff.txt",
        12,
        ArtifactCompleteness::Partial,
    );
    tool_result.output_bytes = Some(40);
    tool_result.preview = Some("head".to_owned());
    tool_result.created_at_ms = 7;
    let cases = [
        (
            user("hello"),
            "{\"user\":{\"text\":\"hello\",\"images\":[],\"work_id\":null}}",
        ),
        (
            ConversationEvent::Assistant(replayed.clone()),
            "{\"assistant\":{\"text\":\"\",\"provider_replay\":{\"source\":{\"provider\":\"gateway\",\"model\":\"test\"},\"parts_json\":\"[{\\\"type\\\":\\\"reasoning\\\",\\\"text\\\":\\\"\\\"}]\"},\"standalone_response\":true}}",
        ),
        (
            ConversationEvent::ToolCall(ToolCallEvent::new(
                "call-1",
                "shell",
                "{\"command\":\"ls\"}",
                ToolArgumentIntegrity::NonObjectJson,
            )),
            "{\"tool_call\":{\"call_id\":\"call-1\",\"tool_name\":\"shell\",\"arguments_json\":\"{\\\"command\\\":\\\"ls\\\"}\",\"argument_integrity\":\"non_object_json\",\"provisional_id\":null,\"provider_result\":null,\"final_identity\":\"valid\",\"provenance\":\"fx_local\"}}",
        ),
        (
            ConversationEvent::ToolResult(tool_result),
            "{\"tool_result\":{\"call_id\":\"call-1\",\"tool_name\":\"shell\",\"status\":\"failure\",\"artifact_ref\":\"result-shell-0011223344556677-8899aabbccddeeff.txt\",\"tool_image_handle\":null,\"output_bytes\":40,\"stored_bytes\":12,\"completeness\":\"partial\",\"preview\":\"head\",\"provider_native\":false,\"created_at_ms\":7,\"permission_feedback\":[],\"committed_file_presentation\":null,\"command_replay_ref\":null,\"command_replay_bytes\":null,\"command_process_presentation\":null,\"terminal_action_presentation\":null}}",
        ),
        (
            ConversationEvent::Steering(SteeringEvent {
                text: "also check tests".to_owned(),
            }),
            "{\"steering\":{\"text\":\"also check tests\"}}",
        ),
        (
            completed(),
            "{\"turn_completed\":{\"files\":[],\"turn_summary\":null}}",
        ),
        (
            checkpoint(0),
            "{\"context_checkpoint\":{\"covers_through_seq\":0,\"summary\":\"summary\"}}",
        ),
    ];
    for (event, shape) in cases {
        let frame = encode(5, &event);
        assert_eq!(
            frame,
            format!("{{\"schema_version\":3,\"seq\":5,\"timestamp_ms\":1,\"event\":{shape}}}\n")
        );
        assert_eq!(decode(&frame).unwrap(), event);
    }
    replayed.provider_replay = None;
    assert!(decode(&encode(1, &ConversationEvent::Assistant(replayed))).is_ok());
}

#[test]
fn review_feedback_defaults_old_records_and_is_never_written() {
    let frame = "{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":1,\"event\":{\"tool_result\":{\"call_id\":\"call-review\",\"tool_name\":\"shell\",\"status\":\"failure\",\"artifact_ref\":\"result.txt\",\"stored_bytes\":0,\"completeness\":\"complete\",\"preview\":\"Security review held this action.\"}}}\n";
    let event = decode(frame).unwrap();
    let ConversationEvent::ToolResult(result) = &event else {
        panic!("expected a tool result");
    };
    assert_eq!(
        result.preview.as_deref(),
        Some("Security review held this action.")
    );
    assert!(!encode(1, &event).contains("review_feedback"));
    let explicit = frame.replace(
        "\"stored_bytes\"",
        "\"review_feedback\":false,\"stored_bytes\"",
    );
    assert!(decode(&explicit).is_ok());
    let held = frame.replace(
        "\"stored_bytes\"",
        "\"review_feedback\":true,\"stored_bytes\"",
    );
    assert_eq!(decode(&held), Err(SessionError::InvalidConversationFrame));
}

#[test]
fn frames_with_unported_upstream_content_are_rejected_not_dropped() {
    let base = "{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":1,\"event\":";
    for event in [
        "{\"user\":{\"text\":\"x\",\"images\":[{\"path\":\"/a.png\",\"media_type\":\"image/png\"}]}}",
        "{\"user\":{\"text\":\"x\",\"work_id\":\"w\"}}",
        "{\"tool_call\":{\"call_id\":\"c\",\"tool_name\":\"t\",\"arguments_json\":\"{}\",\"provenance\":\"provider_executed\"}}",
        "{\"tool_call\":{\"call_id\":\"c\",\"tool_name\":\"t\",\"arguments_json\":\"{}\",\"final_identity\":\"empty\"}}",
        "{\"tool_call\":{\"call_id\":\"c\",\"tool_name\":\"t\",\"arguments_json\":\"{}\",\"provisional_id\":\"p\"}}",
        "{\"tool_call\":{\"call_id\":\"c\",\"tool_name\":\"t\",\"arguments_json\":\"{}\",\"argument_integrity\":\"bogus\"}}",
        "{\"interrupted\":{\"reason\":\"failed\",\"cancellation_origin\":\"compaction\"}}",
        "{\"interrupted\":{\"reason\":\"failed\",\"cancellation_origin\":0}}",
        "{\"interrupted\":{\"reason\":\"stopped\"}}",
        "{\"turn_completed\":{\"files\":[{\"path\":\"a\"}]}}",
        "{\"turn_completed\":{\"turn_summary\":{\"started_at_ms\":1}}}",
        "{\"tool_result\":{\"call_id\":\"c\",\"tool_name\":\"t\",\"status\":\"success\",\"artifact_ref\":\"r\",\"stored_bytes\":0,\"completeness\":\"complete\",\"provider_native\":true}}",
        "{\"tool_result\":{\"call_id\":\"c\",\"tool_name\":\"t\",\"status\":\"success\",\"artifact_ref\":\"r\",\"stored_bytes\":0,\"completeness\":\"complete\",\"permission_feedback\":[\"no\"]}}",
    ] {
        let frame = format!("{base}{event}}}\n");
        assert_eq!(
            decode(&frame),
            Err(SessionError::InvalidConversationFrame),
            "{frame}"
        );
    }
}

#[test]
fn malformed_frames_are_rejected() {
    let valid = "{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":1,\"event\":{\"steering\":{\"text\":\"x\"}}}\n";
    assert!(decode(valid).is_ok());
    assert!(decode(&valid.replace("\"schema_version\":3,", "")).is_ok());
    let invalid = [
        String::new(),
        "\n".to_owned(),
        valid.trim_end().to_owned(),
        valid.replace("\"schema_version\":3", "\"schema_version\":2"),
        valid.replace("\"schema_version\":3", "\"schema_version\":4"),
        valid.replace("\"seq\":1", "\"seq\":0"),
        valid.replace("\"seq\":1", "\"seq\":-1"),
        valid.replace("\"timestamp_ms\":1", "\"timestamp_ms\":-1"),
        valid.replace("\"steering\"", "\"reasoning\""),
        valid.replace("\"text\":\"x\"", "\"text\":\"\""),
        valid.replace("\"text\":\"x\"", "\"text\":\"x\",\"extra\":1"),
        valid.replace("\"seq\":1", "\"seq\":1,\"seq\":2"),
        valid.replace("}}}", "},\"user\":{\"text\":\"y\"}}}"),
        valid.replace("\"x\"", "\"\\ud800\""),
        "{}\n".to_owned(),
        "[]\n".to_owned(),
        "{\"schema_version\":3}\n".to_owned(),
    ];
    for frame in invalid {
        assert_eq!(
            decode(&frame),
            Err(SessionError::InvalidConversationFrame),
            "{frame:?}"
        );
    }
    assert_eq!(
        decode_conversation_frame(b"{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":1,\"event\":{\"steering\":{\"text\":\"\xff\"}}}\n")
            .map(|_| ()),
        Err(SessionError::InvalidConversationFrame)
    );
}

#[test]
fn event_shapes_bound_identities_previews_and_replays() {
    let long_identity = "i".repeat(MAX_IDENTITY_BYTES + 1);
    let mut oversized_preview = ToolResultEvent::new(
        "c",
        "t",
        ToolResultStatus::Success,
        "r",
        0,
        ArtifactCompleteness::Unknown,
    );
    oversized_preview.preview = Some("p".repeat(MAX_PREVIEW_BYTES + 1));
    let mut negative_creation = oversized_preview.clone();
    negative_creation.preview = None;
    negative_creation.created_at_ms = -1;
    let empty_replay = AssistantEvent {
        text: "x".to_owned(),
        provider_replay: Some(SavedReplay {
            source: SavedReplaySource {
                provider: SavedProvider::new(ProviderId::Codex, None).unwrap(),
                model: "gpt".to_owned(),
            },
            parts_json: String::new(),
        }),
        standalone_response: false,
    };
    for event in [
        user(""),
        call(&long_identity, "t"),
        call("c", ""),
        ConversationEvent::ToolCall(ToolCallEvent::new(
            "c",
            "t",
            "",
            ToolArgumentIntegrity::Valid,
        )),
        ConversationEvent::ToolResult(oversized_preview),
        ConversationEvent::ToolResult(negative_creation),
        ConversationEvent::Assistant(empty_replay),
        ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
            covers_through_seq: 0,
            summary: String::new(),
        }),
    ] {
        assert_eq!(
            encode_conversation_frame(1, 1, &event),
            Err(SessionError::InvalidConversationEvent),
            "{event:?}"
        );
    }
    assert_eq!(
        encode_conversation_frame(0, 1, &user("x")),
        Err(SessionError::InvalidConversationEvent)
    );
    assert_eq!(
        encode_conversation_frame(1, -1, &user("x")),
        Err(SessionError::InvalidConversationEvent)
    );
    assert!(encode_conversation_frame(1, 1, &call(&"i".repeat(MAX_IDENTITY_BYTES), "t")).is_ok());
}

#[test]
fn event_frame_cap_is_inclusive_of_the_required_newline() {
    let prefix =
        "{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":1,\"event\":{\"user\":{\"text\":\"";
    let suffix = "\",\"images\":[],\"work_id\":null}}}\n";
    let text = "x".repeat(EVENT_FRAME_MAX_BYTES - prefix.len() - suffix.len());
    let frame = encode_conversation_frame(1, 1, &user(&text)).unwrap();
    assert_eq!(frame.len(), EVENT_FRAME_MAX_BYTES);
    assert!(decode_conversation_frame(&frame).is_ok());
    let longer = format!("{text}x");
    assert_eq!(
        encode_conversation_frame(1, 1, &user(&longer)),
        Err(SessionError::EventFrameTooLarge)
    );
    let mut oversized = frame;
    oversized.insert(0, b' ');
    assert!(decode_conversation_frame(&oversized).is_err());
}

#[test]
fn conversation_transition_validates_sequence_tool_identity_and_checkpoint_safety() {
    assert!(apply(&ConversationState::default(), &checkpoint(0)).is_ok());
    let pending = state_after(&[user("request"), call("call-shell", "shell")]);
    assert!(apply(&pending, &result("call-shell", "shell")).is_ok());
    assert_eq!(
        apply(&pending, &result("call-shell", "read_file")),
        Err(SessionError::ToolIdentityMismatch)
    );
    assert_eq!(
        apply(&pending, &result("missing", "shell")),
        Err(SessionError::OrphanToolResult)
    );
    assert_eq!(
        apply(&pending, &call("call-shell", "shell")),
        Err(SessionError::DuplicateToolCall)
    );
    assert_eq!(
        apply(&pending, &checkpoint(2)),
        Err(SessionError::UnresolvedToolCall)
    );
    assert_eq!(
        apply(&pending, &completed()),
        Err(SessionError::UnresolvedToolCall)
    );
    assert!(apply(&pending, &checkpoint(1)).is_ok());
    let checkpointed = state_after(&[user("request"), call("call-shell", "shell"), checkpoint(1)]);
    assert!(apply(&checkpointed, &checkpoint(1)).is_ok());
    assert_eq!(
        apply(&checkpointed, &checkpoint(0)),
        Err(SessionError::InvalidCheckpointCoverage)
    );
    assert_eq!(
        apply(&checkpointed, &checkpoint(4)),
        Err(SessionError::InvalidCheckpointCoverage)
    );
    let mut skipped = pending.clone();
    assert_eq!(
        skipped.apply(pending.last_seq() + 2, 10, &assistant("skipped sequence")),
        Err(SessionError::OutOfOrderConversationEvent)
    );
    assert_eq!(
        skipped.apply(pending.last_seq() + 1, -1, &assistant("x")),
        Err(SessionError::InvalidConversationEvent)
    );
    assert_eq!(skipped, pending);
}

#[test]
fn checkpoint_coverage_never_ends_between_a_call_and_its_result() {
    let answered = state_after(&[
        user("request"),
        assistant(""),
        call("a", "shell"),
        call("b", "shell"),
        result("b", "shell"),
        result("a", "shell"),
        assistant("done"),
        completed(),
    ]);
    for coverage in [3, 4, 5] {
        assert_eq!(
            apply(&answered, &checkpoint(coverage)),
            Err(SessionError::InvalidCheckpointCoverage),
            "{coverage}"
        );
    }
    for coverage in [0, 1, 2, 6, 7, 8] {
        assert!(
            apply(&answered, &checkpoint(coverage)).is_ok(),
            "{coverage}"
        );
    }

    let mid_turn = state_after(&[
        user("request"),
        call("a", "shell"),
        result("a", "shell"),
        checkpoint(3),
        call("b", "shell"),
        result("b", "shell"),
    ]);
    assert_eq!(mid_turn.answered_tool_spans.len(), 1);
    assert!(apply(&mid_turn, &checkpoint(4)).is_ok());
    assert_eq!(
        apply(&mid_turn, &checkpoint(5)),
        Err(SessionError::InvalidCheckpointCoverage)
    );
    assert!(apply(&mid_turn, &checkpoint(6)).is_ok());

    let mut rewound = mid_turn.clone();
    rewound.rewind_open_turn(4, true);
    assert!(rewound.answered_tool_spans.is_empty());

    let abandoned = state_after(&[
        user("request"),
        call("a", "shell"),
        ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None)),
    ]);
    assert!(apply(&abandoned, &checkpoint(2)).is_ok());
}

#[test]
fn turns_open_with_a_user_and_close_once() {
    let idle = ConversationState::default();
    for event in [assistant("x"), call("c", "t"), completed()] {
        assert_eq!(
            apply(&idle, &event),
            Err(SessionError::InvalidConversationFrame)
        );
    }
    let open = state_after(&[user("one")]);
    assert!(open.turn_open());
    assert_eq!(
        apply(&open, &user("two")),
        Err(SessionError::InvalidConversationFrame)
    );
    let interrupted = state_after(&[
        user("one"),
        call("c", "t"),
        ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None)),
    ]);
    assert!(!interrupted.turn_open());
    assert!(!interrupted.has_pending_tool_calls());
    assert!(state_after(&[user("one"), checkpoint(1)]).turn_open());
}

#[test]
fn the_decoder_stays_bounded_under_corrupted_bytes() {
    let valid = encode(3, &result("call", "shell")).into_bytes();
    for end in 0..valid.len() {
        let mut cut = valid[..end].to_vec();
        cut.push(b'\n');
        let _ = decode_conversation_frame(&cut);
    }
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..2_000 {
        let mut bytes = valid.clone();
        for _ in 0..4 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let index = usize::try_from(seed % u64::try_from(bytes.len() - 1).unwrap()).unwrap();
            bytes[index] = u8::try_from(seed >> 56).unwrap();
        }
        let _ = decode_conversation_frame(&bytes);
    }
}
