use ofx_contract::ReplaySource;

use super::*;
use crate::session_event::FileEvidenceAction;

const IDENTITY: &str = "abababababababababababababababababababababababababababababababab";

fn checkpoint() -> RecoveryCheckpoint {
    RecoveryCheckpoint {
        user: "fix the build".to_owned(),
        assistant_source: "Looking at".to_owned(),
        execution: SavedExecution {
            tool_steps: vec![SavedToolStep {
                assistant: Some("Reading.".to_owned()),
                durable_replay: None,
                provider_replay: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId::new("call_1"),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"a.rs\"}".to_owned(),
                }],
                tool_results: vec![SavedToolResult {
                    tool_call_id: "call_1".to_owned(),
                    tool_name: "read_file".to_owned(),
                    status: ToolResultStatus::Success,
                    output: "fn main() {}".to_owned(),
                    output_handle: None,
                    preview: None,
                    output_bytes: 12,
                    stored_output_bytes: 12,
                    truncated: false,
                }],
            }],
            files: vec![FileEvidence {
                path: "a.rs".to_owned(),
                new_path: None,
                tool_call_id: "call_1".to_owned(),
                tool_name: "read_file".to_owned(),
                action: FileEvidenceAction::Read,
                status: ToolResultStatus::Success,
                model_view_covers_full_file: true,
                stale: false,
            }],
            steering: vec![SavedSteering {
                text: "also tests".to_owned(),
                assistant_prefix: None,
                after_tool_step_count: 1,
            }],
        },
    }
}

fn upstream_checkpoint() -> String {
    format!(
        "{{\"version\":2,\"turn_id\":7,\"user\":{{\"text\":\"fix the build\",\"images\":[]}},\"assistant_source\":\"Looking at\",\"execution\":{{\"schema_version\":10,\"tool_steps\":[{{\"assistant\":\"Reading.\",\"provider_replay\":null,\"tool_calls\":[{{\"id\":\"call_1\",\"name\":\"read_file\",\"arguments_json\":\"{{\\\"path\\\":\\\"a.rs\\\"}}\",\"provider_result\":null}}],\"tool_results\":[{{\"tool_call_id\":\"call_1\",\"tool_name\":\"read_file\",\"status\":\"success\",\"output\":\"fn main() {{}}\",\"output_handle\":null,\"preview\":null,\"output_bytes\":12,\"stored_output_bytes\":12,\"truncated\":false,\"provider_native\":false,\"review_feedback\":false,\"created_at_ms\":5,\"permission_feedback\":[],\"committed_file_presentation\":null,\"command_output_replay\":null,\"command_process_presentation\":null,\"terminal_action_presentation\":null}}]}}],\"files\":[{{\"path\":\"a.rs\",\"new_path\":null,\"tool_call_id\":\"call_1\",\"tool_name\":\"read_file\",\"action\":\"read\",\"status\":\"success\",\"model_view_covers_full_file\":true,\"stale\":false}}],\"steering\":[{{\"text\":\"also tests\",\"assistant_prefix\":null,\"after_tool_step_count\":1}}],\"turn_summary\":null}},\"cause\":\"response_interrupted\",\"action\":\"continuing_response\",\"tool_state\":\"confirmed\",\"authority\":{{\"provider\":\"codex\",\"model\":\"gpt-5.4\",\"credential_source\":\"chatgpt_subscription\",\"credential_identity\":\"{IDENTITY}\"}},\"requested_fast_mode\":false,\"fast_mode\":true,\"max_provider_attempts\":10,\"consumed_provider_attempts\":1,\"outstanding_reservation\":false}}"
    )
}

fn file_with(checkpoint: &str, seq: u64) -> Vec<u8> {
    format!("{{\"conversation_seq\":{seq},\"checkpoint\":{checkpoint}}}\n").into_bytes()
}

fn decoded(text: &str) -> RecoveryCheckpoint {
    decode_recovery_file(&file_with(text, 1), 1)
        .unwrap()
        .unwrap()
}

#[test]
fn upstream_recovery_json_reads_back_for_its_own_sequence_only() {
    let file = file_with(&upstream_checkpoint(), 12);
    assert_eq!(decode_recovery_file(&file, 12), Ok(Some(checkpoint())));
    assert_eq!(decode_recovery_file(&file, 13), Ok(None));
    assert_eq!(
        decode_recovery_file(&file_with("\"not a checkpoint\"", 3), 13),
        Ok(None)
    );
    assert_eq!(
        decode_recovery_file(&file, 11),
        Err(SessionError::InvalidRecoveryCheckpoint)
    );
}

#[test]
fn every_upstream_tag_and_credential_form_is_accepted() {
    let base = upstream_checkpoint();
    for cause in CAUSES {
        decoded(&base.replace("response_interrupted", cause));
    }
    for action in ACTIONS {
        decoded(&base.replace("continuing_response", action));
    }
    for tool_state in TOOL_STATES {
        decoded(&base.replace("\"confirmed\"", &format!("\"{tool_state}\"")));
    }
    for source in CREDENTIAL_SOURCES {
        decoded(&base.replace("chatgpt_subscription", source));
    }
    decoded(&base.replace(&format!("\"{IDENTITY}\""), "null"));
    decoded(&base.replace(
        &format!(
            "\"credential_source\":\"chatgpt_subscription\",\"credential_identity\":\"{IDENTITY}\""
        ),
        "\"credential_source\":null,\"credential_identity\":null",
    ));
    decoded(&base.replace(
        "\"provider\":\"codex\"",
        &format!("\"provider\":{{\"name\":\"portkey\",\"binding\":\"{IDENTITY}\"}}"),
    ));
}

#[test]
fn durable_bytes_steering_prefixes_renames_and_replays_read_back() {
    let text = upstream_checkpoint()
        .replace(
            "\"assistant_source\":\"Looking at\"",
            "\"assistant_source\":{\"encoding\":\"base64\",\"data\":\"TG9va2luZyBhdA==\"}",
        )
        .replace(
            "\"assistant_prefix\":null",
            "\"assistant_prefix\":\"Let me check.\"",
        )
        .replace(
            "\"new_path\":null",
            "\"new_path\":\"b.rs\"",
        )
        .replace(
            "\"provider_replay\":null",
            "\"provider_replay\":{\"source\":{\"provider\":\"codex\",\"model\":\"gpt-5.4\"},\"parts_json\":\"[]\"}",
        );
    let decoded = decoded(&text);
    assert_eq!(decoded.assistant_source, "Looking at");
    assert_eq!(
        decoded.execution.steering[0].assistant_prefix.as_deref(),
        Some("Let me check.")
    );
    assert_eq!(decoded.execution.files[0].new_path.as_deref(), Some("b.rs"));
    assert_eq!(
        decoded.execution.tool_steps[0].provider_replay,
        Some(ProviderReplay {
            source: ReplaySource {
                provider: "codex".to_owned(),
                model: "gpt-5.4".to_owned(),
            },
            parts_json: "[]".to_owned(),
        })
    );
}

#[test]
fn malformed_arguments_read_back_as_an_empty_object_as_upstream_repairs_them() {
    let text = upstream_checkpoint().replace(
        "\"arguments_json\":\"{\\\"path\\\":\\\"a.rs\\\"}\"",
        "\"arguments_json\":\"{\\\"path\\\":\"",
    );
    assert_eq!(
        decoded(&text).execution.tool_steps[0].tool_calls[0].arguments,
        "{}"
    );
}

#[test]
fn the_interrupted_turn_carries_the_partial_reply_steps_and_steering() {
    let checkpoint = checkpoint();
    let turn = checkpoint.interrupted_turn();
    assert_eq!(turn.user, "fix the build");
    assert_eq!(
        turn.end,
        TurnEnd::Stopped {
            reason: TurnStop::Failed,
            partial: "Looking at",
        }
    );
    assert_eq!(turn.steps.len(), 1);
    assert_eq!(turn.steps[0].assistant, "Reading.");
    assert_eq!(turn.steps[0].tool_results[0].output, "fn main() {}");
    assert_eq!(
        turn.steering,
        [HistorySteering {
            text: "also tests",
            assistant_prefix: "",
            after_tool_step_count: 1,
        }]
    );
    assert_eq!(checkpoint.into_files().len(), 1);
}

#[test]
fn checkpoints_upstream_rejects_or_oh_fx_cannot_hold_are_invalid() {
    let base = upstream_checkpoint();
    let cases = [
        base.replace("\"version\":2", "\"version\":1"),
        base.replace("\"version\":2", "\"version\":3"),
        base.replace("\"images\":[]", "\"images\":[{\"id\":1}]"),
        base.replace("\"images\":[]", "\"images\":[],\"work_id\":\"w\""),
        base.replace("\"text\":\"fix the build\"", "\"text\":\"\""),
        base.replace(
            "\"text\":\"fix the build\"",
            "\"text\":{\"encoding\":\"base64\",\"data\":\"/w==\"}",
        ),
        base.replace(
            "\"text\":\"fix the build\"",
            "\"text\":{\"data\":\"Zml4\",\"encoding\":\"base64\"}",
        ),
        base.replace("\"schema_version\":10", "\"schema_version\":9"),
        base.replace(
            "\"turn_summary\":null",
            "\"turn_summary\":{\"started_at_ms\":1}",
        ),
        base.replace("\"provider_result\":null", "\"provider_result\":\"r\""),
        base.replace("\"provider_native\":false", "\"provider_native\":true"),
        base.replace("\"review_feedback\":false", "\"review_feedback\":true"),
        base.replace(
            "\"permission_feedback\":[]",
            "\"permission_feedback\":[\"no\"]",
        ),
        base.replace(
            "\"command_output_replay\":null",
            "\"command_output_replay\":{\"kind\":\"unavailable\"}",
        ),
        base.replace(
            "\"terminal_action_presentation\":null",
            "\"terminal_action_presentation\":null,\"tool_image_handle\":\"i\"",
        ),
        base.replace("\"status\":\"success\",\"output\"", "\"status\":\"done\",\"output\""),
        base.replace("\"action\":\"read\"", "\"action\":\"peek\""),
        base.replace("\"cause\":\"response_interrupted\"", "\"cause\":\"bored\""),
        base.replace("\"action\":\"continuing_response\"", "\"action\":\"waiting\""),
        base.replace("\"tool_state\":\"confirmed\"", "\"tool_state\":\"maybe\""),
        base.replace(
            "\"credential_source\":\"chatgpt_subscription\"",
            "\"credential_source\":null",
        ),
        base.replace(
            "\"credential_source\":\"chatgpt_subscription\"",
            "\"credential_source\":\"keychain\"",
        ),
        base.replace(IDENTITY, &IDENTITY.to_uppercase()),
        base.replace(IDENTITY, "abab"),
        base.replace("\"provider\":\"codex\"", "\"provider\":\"nowhere\""),
        base.replace(
            "\"after_tool_step_count\":1",
            "\"after_tool_step_count\":2",
        ),
        base.replace(
            "\"steering\":[{\"text\":\"also tests\",\"assistant_prefix\":null,\"after_tool_step_count\":1}]",
            "\"steering\":[{\"text\":\"a\",\"assistant_prefix\":null,\"after_tool_step_count\":1},{\"text\":\"b\",\"assistant_prefix\":null,\"after_tool_step_count\":0}]",
        ),
        base.replace("\"fast_mode\":true", "\"fast_mode\":true,\"extra\":1"),
        base.replace("\"turn_id\":7,", ""),
        base.replace("\"stale\":false", "\"stale\":false,\"extra\":1"),
        base.replace("\"new_path\":null,", ""),
        base.replace("\"files\":[{\"path\":\"a.rs\"", "\"files\":[{\"path\":\"\""),
        base.replace("\"tool_call_id\":\"call_1\",\"tool_name\":\"read_file\",\"action\"", "\"tool_call_id\":\"\",\"tool_name\":\"read_file\",\"action\""),
    ];
    for text in cases {
        assert_eq!(
            decode_recovery_file(&file_with(&text, 1), 1),
            Err(SessionError::InvalidRecoveryCheckpoint),
            "{text:.300}"
        );
    }
    let invalid_files = [
        format!("{{\"conversation_seq\":1,\"checkpoint\":{base},\"extra\":1}}\n"),
        format!("{{\"checkpoint\":{base}}}\n"),
        format!("{{\"conversation_seq\":-1,\"checkpoint\":{base}}}\n"),
        "{\"conversation_seq\":1}\n".to_owned(),
        "not json".to_owned(),
        "[]".to_owned(),
    ];
    for file in invalid_files {
        assert_eq!(
            decode_recovery_file(file.as_bytes(), 1),
            Err(SessionError::InvalidRecoveryCheckpoint),
            "{file:.300}"
        );
    }
}

#[test]
fn spilled_outputs_restore_from_the_result_store_or_say_they_are_unavailable() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open_or_create(&root.path().join("session")).unwrap();
    let handle = crate::result_store::make_handle("call_1", "read_file", "fn main() {}");
    crate::result_store::store_result(&dir, &handle, "fn main() {}").unwrap();
    let spilled = |handle: &str, truncated: bool| {
        upstream_checkpoint()
            .replace("\"output\":\"fn main() {}\"", "\"output\":\"\"")
            .replace(
                "\"output_handle\":null,\"preview\":null",
                &format!("\"output_handle\":\"{handle}\",\"preview\":\"fn main\""),
            )
            .replace("\"truncated\":false", &format!("\"truncated\":{truncated}"))
    };
    let output = |text: &str| {
        let mut checkpoint = decoded(text);
        checkpoint.restore_outputs(&dir);
        checkpoint.execution.tool_steps[0].tool_results[0]
            .output
            .clone()
    };
    assert_eq!(output(&spilled(&handle, false)), "fn main() {}");
    assert_eq!(
        output(&spilled("result-read_file-0-0.txt", false)),
        RESULT_UNAVAILABLE
    );
    assert_eq!(
        output(&spilled(&handle, true)),
        format_stored_result_output(&handle, "fn main", 12)
    );
    assert_eq!(output(&upstream_checkpoint()), "fn main() {}");
}
