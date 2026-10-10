use ofx_config::ProviderId;
use ofx_contract::{
    ChatMessage, HistorySteering, HistoryStep, HistoryTurn, ModelRecoveryAction,
    ModelRecoveryCause, RecordedOutput, RecoveryPoint, RecoveryProgress, RecoveryToolState,
    ReplaySource, StepResult, ToolCallId, TurnId, TurnSummary, TurnTokenProgress,
};

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
                provider_replay: None,
                tool_calls: vec![ToolCall::new("call_1", "read_file", "{\"path\":\"a.rs\"}")],
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
                    process: None,
                    permission_feedback: Vec::new(),
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
            turn_summary: None,
        },
        cause: None,
        tool_state: RecoveryToolState::Confirmed,
        strategy: RecoveryStrategy::ContinueAfterTool,
        route: RecoveryRoute {
            provider: SavedProvider::new(ProviderId::Codex, None).unwrap(),
            model: "gpt-5.4".to_owned(),
            credential: Some(RouteCredential::saved(
                "chatgpt_subscription",
                Some([0xab; 32]),
            )),
            requested_fast_mode: false,
            fast_mode: true,
            requested_ultrafast_mode: false,
            may_have_sent: true,
        },
        compaction_prepared: false,
        uncertain_tool: false,
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
        decoded(&base.replace("\"confirmed\"", &format!("\"{}\"", tool_state.as_str())));
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
                binding: None,
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

const SUMMARY: &str = "\"turn_summary\":{\"started_at_ms\":1000,\"completed_at_ms\":4500,\"thinking_duration_ms\":1200,\"turn_duration_ms\":3500,\"token_progress\":{\"input_tokens\":1234,\"output_tokens\":340,\"input_exact\":true,\"output_exact\":false}}";

#[test]
fn a_checkpoints_turn_summary_is_read_as_upstream_writes_it() {
    let text = upstream_checkpoint().replace("\"turn_summary\":null", SUMMARY);
    assert_eq!(
        decoded(&text).turn_summary(),
        Some(TurnSummary {
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
        })
    );
    assert_eq!(decoded(&upstream_checkpoint()).turn_summary(), None);
    for summary in [
        SUMMARY.replace("1000", "-1"),
        SUMMARY.replace("4500", "999"),
        SUMMARY.replace("\"output_exact\":false", "\"output_exact\":0"),
        SUMMARY.replace(",\"output_exact\":false", ""),
    ] {
        let text = upstream_checkpoint().replace("\"turn_summary\":null", &summary);
        assert_eq!(
            decode_recovery_file(&file_with(&text, 1), 1),
            Err(SessionError::InvalidRecoveryCheckpoint),
            "{summary}"
        );
    }
}

#[test]
fn checkpoints_upstream_rejects_or_oh_fx_cannot_hold_are_invalid() {
    let base = upstream_checkpoint();
    let cases = [
        base.replace("\"version\":2", "\"version\":1"),
        base.replace(
            "\"tool_results\":[{\"tool_call_id\":\"call_1\"",
            "\"tool_results\":[{\"tool_call_id\":\"call_9\"",
        ),
        base.replace(
            "\"tool_results\":[{\"tool_call_id\":\"call_1\",\"tool_name\":\"read_file\"",
            "\"tool_results\":[{\"tool_call_id\":\"call_1\",\"tool_name\":\"write_file\"",
        ),
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
fn a_repeated_key_invalidates_the_recovery_file_even_for_an_earlier_sequence() {
    let base = upstream_checkpoint();
    let repeated_inside = |field: &str| {
        let text = base.replacen(field, &format!("{field}{field}"), 1);
        format!("{{\"conversation_seq\":1,\"checkpoint\":{text}}}\n")
    };
    let files = [
        format!("{{\"conversation_seq\":1,\"conversation_seq\":1,\"checkpoint\":{base}}}\n"),
        format!("{{\"conversation_seq\":1,\"checkpoint\":{base},\"checkpoint\":{base}}}\n"),
        format!("{{\"conversation_seq\":1,\"conversation_seq\":1,\"checkpoint\":{base}"),
        repeated_inside("\"turn_id\":7,"),
        repeated_inside("\"output_bytes\":12,"),
        repeated_inside("\"credential_source\":\"chatgpt_subscription\","),
    ];
    for file in files {
        for current in [1, 2] {
            assert_eq!(
                decode_recovery_file(file.as_bytes(), current),
                Err(SessionError::InvalidRecoveryCheckpoint),
                "{file:.300}"
            );
        }
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

const CONFIGURED_IDENTITY: &str =
    "40122b758656199048961e6e8369383c25ebcdeddced75b64ad736e527014da8";
const ACCOUNT_IDENTITY: &str = "0e5c498c1df280148a53f73b4722f0a5b602ca6ea599ca03eea8aad6a783ee39";

fn with_credential(
    source: &str,
    identity: &str,
    consumed: u64,
    outstanding: bool,
) -> RecoveryCheckpoint {
    decoded(
        &upstream_checkpoint()
            .replace("chatgpt_subscription", source)
            .replace(&format!("\"{IDENTITY}\""), identity)
            .replace(
                "\"consumed_provider_attempts\":1",
                &format!("\"consumed_provider_attempts\":{consumed}"),
            )
            .replace(
                "\"outstanding_reservation\":false",
                &format!("\"outstanding_reservation\":{outstanding}"),
            ),
    )
}

#[test]
fn a_possibly_sent_request_continues_only_under_the_credential_that_sent_it() {
    let configured = with_credential(
        "configured",
        &format!("\"{CONFIGURED_IDENTITY}\""),
        1,
        false,
    );
    assert!(configured.authorizes(RouteCredential::configured()));
    assert!(!configured.authorizes(RouteCredential::chatgpt_subscription("acct_1")));
    let account = with_credential(
        "chatgpt_subscription",
        &format!("\"{ACCOUNT_IDENTITY}\""),
        0,
        true,
    );
    assert!(account.authorizes(RouteCredential::chatgpt_subscription("acct_1")));
    for other in ["acct_2", ""] {
        assert!(!account.authorizes(RouteCredential::chatgpt_subscription(other)));
    }
    assert!(!account.authorizes(RouteCredential::configured()));
    let unidentified = with_credential("configured", "null", 1, false);
    assert!(!unidentified.authorizes(RouteCredential::configured()));
    let unsent = with_credential("configured", "null", 0, false);
    assert!(unsent.authorizes(RouteCredential::chatgpt_subscription("acct_9")));
}

#[test]
fn the_recovery_strategy_follows_the_saved_tool_state_and_cause() {
    let base = upstream_checkpoint();
    let cases = [
        (
            "response_interrupted",
            "confirmed",
            "Looking at",
            RecoveryStrategy::ContinueAfterTool,
        ),
        (
            "response_interrupted",
            "proven_unexecuted",
            "",
            RecoveryStrategy::RegenerateTool,
        ),
        (
            "response_interrupted",
            "uncertain",
            "",
            RecoveryStrategy::ReconcileTool,
        ),
        (
            "response_interrupted",
            "none",
            "Looking at",
            RecoveryStrategy::ContinueResponse,
        ),
        ("rate_limited", "none", "", RecoveryStrategy::RetryRequest),
        (
            "request_limit_reached",
            "uncertain",
            "Looking at",
            RecoveryStrategy::RetryRequest,
        ),
    ];
    for (cause, tool_state, partial, expected) in cases {
        let text = base
            .replace("response_interrupted", cause)
            .replace(
                "\"tool_state\":\"confirmed\"",
                &format!("\"tool_state\":\"{tool_state}\""),
            )
            .replace(
                "\"assistant_source\":\"Looking at\"",
                &format!("\"assistant_source\":\"{partial}\""),
            );
        assert_eq!(
            decoded(&text).strategy,
            expected,
            "{cause} {tool_state} {partial:?}"
        );
    }
}

#[test]
fn a_continuation_keeps_the_saved_fast_mode_only_for_the_same_selection() {
    let codex = SavedProvider::new(ProviderId::Codex, None).unwrap();
    let continued = checkpoint().into_continuation(&codex, "gpt-5.4", false, false);
    assert!(continued.fast_mode);
    assert_eq!(continued.prompt, "fix the build");
    assert_eq!(continued.strategy, RecoveryStrategy::ContinueAfterTool);
    assert_eq!(
        continued.messages[0],
        ChatMessage::Assistant {
            content: Some("Reading.".to_owned()),
            tool_calls: checkpoint().execution.tool_steps[0].tool_calls.clone(),
            provider_replay: None,
        }
    );
    assert_eq!(continued.messages.len(), 3);
    assert!(
        !checkpoint()
            .into_continuation(&codex, "gpt-5.5", false, false)
            .fast_mode
    );
    assert!(
        checkpoint()
            .into_continuation(&codex, "gpt-5.4", true, false)
            .fast_mode
    );
    let gateway = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    assert!(
        !checkpoint()
            .into_continuation(&gateway, "gpt-5.4", false, false)
            .fast_mode
    );
}

#[test]
fn an_ultra_request_is_read_as_upstream_writes_it_and_kept_only_for_the_same_selection() {
    let pair = "\"requested_ultrafast_mode\":true,\"ultrafast_mode\":true,";
    let ultra =
        upstream_checkpoint().replace("\"fast_mode\":true,", &format!("\"fast_mode\":true,{pair}"));
    let codex = SavedProvider::new(ProviderId::Codex, None).unwrap();
    let continued = |text: &str, ultrafast| {
        decoded(text)
            .into_continuation(&codex, "gpt-5.4", false, ultrafast)
            .fast_mode
    };
    assert!(continued(&ultra, true));
    assert!(!continued(&ultra, false));
    assert!(!continued(&upstream_checkpoint(), true));
    let effective_only = ultra.replace(
        "\"requested_ultrafast_mode\":true",
        "\"requested_ultrafast_mode\":false",
    );
    assert!(continued(&effective_only, false));
    for invalid in [
        ultra.replace("\"requested_ultrafast_mode\":true,", ""),
        ultra.replace("\"ultrafast_mode\":true,\"max", "\"max"),
        ultra.replace(
            "\"requested_ultrafast_mode\":true",
            "\"requested_ultrafast_mode\":null",
        ),
        ultra.replace(
            "\"ultrafast_mode\":true,\"max",
            "\"ultrafast_mode\":1,\"max",
        ),
        ultra.replace(
            "\"requested_ultrafast_mode\":true",
            "\"requested_ultrafast_mode\":\"true\"",
        ),
    ] {
        assert_eq!(
            decode_recovery_file(&file_with(&invalid, 1), 1),
            Err(SessionError::InvalidRecoveryCheckpoint),
            "{invalid}"
        );
    }
}

#[test]
fn an_ultra_request_is_written_as_upstream_writes_it() {
    let calls = read_step_calls();
    let mut point = recovery_point(&calls, "fn main() {}");
    point.ultrafast_mode = true;
    let source = CheckpointSource {
        point: &point,
        provider: &SavedProvider::new(ProviderId::Codex, None).unwrap(),
        credential: None,
        replays: vec![None],
        outputs: vec![vec![SavedOutput {
            handle: None,
            preview: None,
        }]],
        files: Vec::new(),
        created_at_ms: 5,
    };
    let written = encode_recovery_file(3, &source).unwrap().unwrap();
    let text = String::from_utf8(written.clone()).unwrap();
    assert!(
        text.contains(
            "\"requested_fast_mode\":false,\"fast_mode\":true,\"requested_ultrafast_mode\":true,\"ultrafast_mode\":true,\"max_provider_attempts\":10,"
        ),
        "{text}"
    );
    let codex = SavedProvider::new(ProviderId::Codex, None).unwrap();
    let read = decode_recovery_file(&written, 3).unwrap().unwrap();
    assert!(
        read.into_continuation(&codex, "gpt-5.4", false, true)
            .fast_mode
    );
}

#[test]
fn a_continuation_keeps_each_restored_results_raw_size_and_process() {
    let codex = SavedProvider::new(ProviderId::Codex, None).unwrap();
    let mut saved = checkpoint();
    let result = &mut saved.execution.tool_steps[0].tool_results[0];
    result.output_bytes = 40;
    result.process = Some(CommandProcessPresentation::ExitCode(3));
    assert_eq!(
        saved
            .into_continuation(&codex, "gpt-5.4", false, false)
            .outputs,
        [RecordedOutput {
            call_id: ToolCallId::new("call_1"),
            bytes: 40,
            whole_file: false,
            process: Some(CommandProcessPresentation::ExitCode(3)),
        }]
    );
}

fn read_step_calls() -> Vec<ToolCall> {
    vec![ToolCall::new("call_1", "read_file", "{\"path\":\"a.rs\"}")]
}

fn recovery_point<'a>(calls: &'a [ToolCall], output: &'a str) -> RecoveryPoint<'a> {
    RecoveryPoint {
        turn_id: TurnId::new(7),
        turn: HistoryTurn {
            user: "fix the build",
            steps: vec![HistoryStep {
                assistant: "Reading.",
                provider_replay: None,
                tool_calls: calls,
                tool_results: vec![StepResult {
                    call_id: "call_1",
                    tool_name: "read_file",
                    output,
                    output_bytes: 12,
                    status: ToolResultStatus::Success,
                    model_view_covers_full_file: false,
                    process: None,
                    permission_feedback: Vec::new(),
                }],
            }],
            steering: vec![HistorySteering {
                text: "also tests",
                assistant_prefix: "",
                after_tool_step_count: 1,
            }],
            files: &[],
            end: TurnEnd::Replied {
                text: "",
                provider_replay: None,
            },
        },
        source: "Looking at",
        cause: ModelRecoveryCause::RateLimited,
        progress: RecoveryProgress::Waiting(ModelRecoveryAction::RetryingRequest),
        tool_state: RecoveryToolState::Confirmed,
        model: "gpt-5.4",
        requested_fast_mode: false,
        fast_mode: true,
        ultrafast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 1,
    }
}

#[test]
fn a_recovery_point_is_written_in_upstream_recovery_json_form() {
    let calls = read_step_calls();
    let point = recovery_point(&calls, "fn main() {}");
    let source = CheckpointSource {
        point: &point,
        provider: &SavedProvider::new(ProviderId::Codex, None).unwrap(),
        credential: Some(RouteCredential::chatgpt_subscription("acct_1")),
        replays: vec![None],
        outputs: vec![vec![SavedOutput {
            handle: None,
            preview: None,
        }]],
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
        created_at_ms: 5,
    };
    let written = encode_recovery_file(12, &source).unwrap().unwrap();
    let expected = upstream_checkpoint()
        .replace(
            "\"cause\":\"response_interrupted\",\"action\":\"continuing_response\"",
            "\"cause\":\"rate_limited\",\"action\":\"retrying_request\"",
        )
        .replace(IDENTITY, ACCOUNT_IDENTITY);
    assert_eq!(
        String::from_utf8(written.clone()).unwrap(),
        String::from_utf8(file_with(&expected, 12)).unwrap()
    );
    let read = decode_recovery_file(&written, 12).unwrap().unwrap();
    assert!(read.authorizes(RouteCredential::chatgpt_subscription("acct_1")));
    assert!(!read.authorizes(RouteCredential::chatgpt_subscription("acct_2")));
    assert_eq!(read.strategy, RecoveryStrategy::ContinueAfterTool);
    assert_eq!(
        read.execution.tool_steps[0].tool_results[0].output,
        "fn main() {}"
    );
}

#[test]
fn a_continuation_restarts_from_the_saved_partial_reply() {
    let continued = decoded(
        &upstream_checkpoint().replace("\"tool_state\":\"confirmed\"", "\"tool_state\":\"none\""),
    )
    .into_continuation(
        &SavedProvider::new(ProviderId::Codex, None).unwrap(),
        "gpt-5.4",
        false,
        false,
    );
    assert_eq!(continued.source, "Looking at");
    assert_eq!(continued.strategy, RecoveryStrategy::ContinueResponse);
    assert_eq!(continued.cause, None);
    let codex = SavedProvider::new(ProviderId::Codex, None).unwrap();
    for (tag, cause) in [
        (
            "network_interrupted",
            ModelRecoveryCause::NetworkInterrupted,
        ),
        ("connectivity_lost", ModelRecoveryCause::ConnectivityLost),
        (
            "provider_stream_timeout",
            ModelRecoveryCause::ProviderStreamTimeout,
        ),
        (
            "provider_unavailable",
            ModelRecoveryCause::ProviderUnavailable,
        ),
        ("rate_limited", ModelRecoveryCause::RateLimited),
    ] {
        let continued = decoded(&upstream_checkpoint().replace("response_interrupted", tag))
            .into_continuation(&codex, "gpt-5.4", false, false);
        assert_eq!(continued.cause, Some(cause), "{tag}");
    }
}

#[test]
fn a_paused_point_and_a_spilled_output_are_written_as_upstream_writes_them() {
    let calls = read_step_calls();
    let mut point = recovery_point(&calls, "fn main() {}");
    point.progress = RecoveryProgress::Paused;
    let source = CheckpointSource {
        point: &point,
        provider: &SavedProvider::new(ProviderId::Codex, None).unwrap(),
        credential: None,
        replays: vec![None],
        outputs: vec![vec![SavedOutput {
            handle: Some("result-read_file-1-2.txt".to_owned()),
            preview: Some("fn".to_owned()),
        }]],
        files: Vec::new(),
        created_at_ms: 5,
    };
    let written = String::from_utf8(encode_recovery_file(3, &source).unwrap().unwrap()).unwrap();
    assert!(written.contains("\"action\":\"paused\""), "{written}");
    assert!(
        written.contains("\"output\":\"\",\"output_handle\":\"result-read_file-1-2.txt\",\"preview\":\"fn\",\"output_bytes\":12,\"stored_output_bytes\":12"),
        "{written}"
    );
    assert!(
        written.contains("\"credential_source\":null,\"credential_identity\":null"),
        "{written}"
    );
}

#[test]
fn a_commands_process_presentation_is_written_and_read_in_upstreams_checkpoint_form() {
    let calls = read_step_calls();
    let mut point = recovery_point(&calls, "fn main() {}");
    point.turn.steps[0].tool_results[0].process = Some(CommandProcessPresentation::ExitCode(1));
    let source = CheckpointSource {
        point: &point,
        provider: &SavedProvider::new(ProviderId::Codex, None).unwrap(),
        credential: None,
        replays: vec![None],
        outputs: vec![vec![SavedOutput {
            handle: None,
            preview: None,
        }]],
        files: Vec::new(),
        created_at_ms: 5,
    };
    let written = encode_recovery_file(3, &source).unwrap().unwrap();
    let text = String::from_utf8(written.clone()).unwrap();
    assert!(
        text.contains(
            "\"command_output_replay\":null,\"command_process_presentation\":{\"kind\":\"exit_code\",\"value\":1},\"terminal_action_presentation\":null"
        ),
        "{text}"
    );
    let read = decode_recovery_file(&written, 3).unwrap().unwrap();
    assert_eq!(
        read.execution.tool_steps[0].tool_results[0].process,
        Some(CommandProcessPresentation::ExitCode(1))
    );
    let upstream = upstream_checkpoint().replace(
        "\"command_process_presentation\":null",
        "\"command_process_presentation\":{\"kind\":\"timed_out\",\"value\":null}",
    );
    assert_eq!(
        decoded(&upstream).execution.tool_steps[0].tool_results[0].process,
        Some(CommandProcessPresentation::TimedOut)
    );
    for invalid in [
        "{\"kind\":\"timed_out\",\"value\":1}",
        "{\"timed_out\":{}}",
        "{\"kind\":\"exit_code\"}",
    ] {
        let text = upstream_checkpoint().replace(
            "\"command_process_presentation\":null",
            &format!("\"command_process_presentation\":{invalid}"),
        );
        assert_eq!(
            decode_recovery_file(&file_with(&text, 1), 1),
            Err(SessionError::InvalidRecoveryCheckpoint),
            "{invalid}"
        );
    }
}

const FEEDBACK: [&str; 2] = ["read it after writing", "then run the tests"];

#[test]
fn approval_feedback_is_written_and_read_in_upstreams_checkpoint_form() {
    let calls = read_step_calls();
    let mut point = recovery_point(&calls, "fn main() {}");
    point.turn.steps[0].tool_results[0].permission_feedback = FEEDBACK.to_vec();
    let source = CheckpointSource {
        point: &point,
        provider: &SavedProvider::new(ProviderId::Codex, None).unwrap(),
        credential: Some(RouteCredential::chatgpt_subscription("acct_1")),
        replays: vec![None],
        outputs: vec![vec![SavedOutput {
            handle: None,
            preview: None,
        }]],
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
        created_at_ms: 5,
    };
    let written = encode_recovery_file(12, &source).unwrap().unwrap();
    let expected = upstream_checkpoint()
        .replace(
            "\"cause\":\"response_interrupted\",\"action\":\"continuing_response\"",
            "\"cause\":\"rate_limited\",\"action\":\"retrying_request\"",
        )
        .replace(IDENTITY, ACCOUNT_IDENTITY)
        .replace(
            "\"permission_feedback\":[]",
            "\"permission_feedback\":[\"read it after writing\",\"then run the tests\"]",
        );
    assert_eq!(
        String::from_utf8(written.clone()).unwrap(),
        String::from_utf8(file_with(&expected, 12)).unwrap()
    );
    let read = decode_recovery_file(&written, 12).unwrap().unwrap();
    assert_eq!(
        read.interrupted_turn().steps[0].tool_results[0].permission_feedback,
        FEEDBACK
    );
    let durable = upstream_checkpoint().replace(
        "\"permission_feedback\":[]",
        "\"permission_feedback\":[\"read it after writing\",{\"encoding\":\"base64\",\"data\":\"dGhlbiBydW4gdGhlIHRlc3Rz\"}]",
    );
    assert_eq!(
        decoded(&durable).execution.tool_steps[0].tool_results[0].permission_feedback,
        FEEDBACK
    );
    for invalid in [
        ":[1]",
        ":\"no\"",
        ":null",
        ":[{\"encoding\":\"base64\",\"data\":\"/w==\"}]",
    ] {
        let text = upstream_checkpoint().replace(
            "\"permission_feedback\":[]",
            &format!("\"permission_feedback\"{invalid}"),
        );
        assert_eq!(
            decode_recovery_file(&file_with(&text, 1), 1),
            Err(SessionError::InvalidRecoveryCheckpoint),
            "{invalid}"
        );
    }
    let missing = upstream_checkpoint().replace(",\"permission_feedback\":[]", "");
    assert_eq!(
        decode_recovery_file(&file_with(&missing, 1), 1),
        Err(SessionError::InvalidRecoveryCheckpoint)
    );
}

#[test]
fn a_continuation_gives_approval_feedback_after_every_result_of_its_step() {
    let mut saved = checkpoint();
    let step = &mut saved.execution.tool_steps[0];
    step.tool_calls
        .push(ToolCall::new("call_2", "read_file", "{\"path\":\"b.rs\"}"));
    let mut second = step.tool_results[0].clone();
    second.tool_call_id = "call_2".to_owned();
    second.output = "fn b() {}".to_owned();
    second.permission_feedback = vec![FEEDBACK[1].to_owned(), String::new()];
    step.tool_results[0].permission_feedback = vec![FEEDBACK[0].to_owned()];
    step.tool_results.push(second);
    let calls = step.tool_calls.clone();
    assert_eq!(
        saved.transcript().entries,
        [
            HistoryEntry::User("fix the build".to_owned()),
            HistoryEntry::Assistant("Reading.".to_owned()),
            HistoryEntry::User(FEEDBACK[0].to_owned()),
            HistoryEntry::User(FEEDBACK[1].to_owned()),
            HistoryEntry::User("also tests".to_owned()),
            HistoryEntry::Assistant("Looking at".to_owned()),
        ]
    );
    let codex = SavedProvider::new(ProviderId::Codex, None).unwrap();
    let tool = |id: &str, content: &str| ChatMessage::Tool {
        call_id: ToolCallId::new(id),
        tool_name: "read_file".to_owned(),
        content: content.to_owned(),
        status: ToolResultStatus::Success,
    };
    assert_eq!(
        saved
            .into_continuation(&codex, "gpt-5.4", false, false)
            .messages,
        [
            ChatMessage::Assistant {
                content: Some("Reading.".to_owned()),
                tool_calls: calls,
                provider_replay: None,
            },
            tool("call_1", "fn main() {}"),
            tool("call_2", "fn b() {}"),
            ChatMessage::permission_feedback(ToolCallId::new("call_1"), FEEDBACK[0]),
            ChatMessage::permission_feedback(ToolCallId::new("call_2"), FEEDBACK[1]),
            ChatMessage::permission_feedback(ToolCallId::new("call_2"), ""),
            ChatMessage::restored_steering("also tests"),
        ]
    );
}
