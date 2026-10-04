use ofx_config::ProviderId;
use ofx_contract::{
    ChatMessage, HistorySteering, HistoryStep, HistoryTurn, ModelRecoveryAction,
    ModelRecoveryCause, RecoveryPoint, RecoveryProgress, ReplaySource, StepResult, TurnId,
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
    let continued = checkpoint().into_continuation(&codex, "gpt-5.4", false);
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
            .into_continuation(&codex, "gpt-5.5", false)
            .fast_mode
    );
    assert!(
        checkpoint()
            .into_continuation(&codex, "gpt-5.4", true)
            .fast_mode
    );
    let gateway = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    assert!(
        !checkpoint()
            .into_continuation(&gateway, "gpt-5.4", false)
            .fast_mode
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
        cause: ModelRecoveryCause::RateLimited,
        progress: RecoveryProgress::Waiting(ModelRecoveryAction::RetryingRequest),
        model: "gpt-5.4",
        requested_fast_mode: false,
        fast_mode: true,
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
        .replace("\"cause\":\"response_interrupted\",\"action\":\"continuing_response\",\"tool_state\":\"confirmed\"", "\"cause\":\"rate_limited\",\"action\":\"retrying_request\",\"tool_state\":\"none\"")
        .replace("\"assistant_source\":\"Looking at\"", "\"assistant_source\":\"\"")
        .replace(IDENTITY, ACCOUNT_IDENTITY);
    assert_eq!(
        String::from_utf8(written.clone()).unwrap(),
        String::from_utf8(file_with(&expected, 12)).unwrap()
    );
    let read = decode_recovery_file(&written, 12).unwrap().unwrap();
    assert!(read.authorizes(RouteCredential::chatgpt_subscription("acct_1")));
    assert!(!read.authorizes(RouteCredential::chatgpt_subscription("acct_2")));
    assert_eq!(read.strategy, RecoveryStrategy::RetryRequest);
    assert_eq!(
        read.execution.tool_steps[0].tool_results[0].output,
        "fn main() {}"
    );
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
