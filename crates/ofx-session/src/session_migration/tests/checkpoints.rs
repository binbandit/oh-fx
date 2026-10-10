use ofx_contract::HistoryEntry;

use super::replacements::{prompts, reply_007, state_007, written};
use super::*;
use crate::result_store::make_handle;
use crate::session_log::read_checkpoint;

const IDENTITY: &str = "abababababababababababababababababababababababababababababababab";

fn checkpoint_007(output: &str) -> String {
    format!(
        "{{\"version\":2,\"turn_id\":2,\"user\":{{\"text\":\"run the tests\",\"images\":[]}},\"assistant_source\":\"Running them now.\",\"execution\":{{\"schema_version\":4,\"tool_steps\":[{{\"assistant\":\"Checking.\",\"tool_calls\":[{{\"id\":\"call_1\",\"name\":\"run_command\",\"arguments_json\":\"{{\\\"command\\\":\\\"cargo test\\\"}}\",\"provider_result\":null}}],\"tool_results\":[{}]}}],\"files\":[]}},\"cause\":\"network_interrupted\",\"action\":\"retrying_request\",\"tool_state\":\"none\",\"authority\":{{\"provider\":\"gateway\",\"model\":\"openai/gpt-5\",\"credential_source\":\"ai_gateway_api_key\",\"credential_identity\":\"{IDENTITY}\"}},\"requested_fast_mode\":false,\"fast_mode\":false,\"max_provider_attempts\":3,\"consumed_provider_attempts\":1,\"outstanding_reservation\":false}}",
        command_result_007(output)
    )
}

fn command_result_007(output: &str) -> String {
    let result = command_result("call_1", output, "null");
    format!(
        "{},\"terminal_action_presentation\":null}}",
        &result[..result.len() - 1]
    )
}

fn set(checkpoint: &str) -> String {
    format!("{{\"checkpoint\":{checkpoint}}}")
}

fn copy_dir(fixture: &Fixture, id: &str) -> PrivateDir {
    PrivateDir::open_existing(&fixture.root.path().join("copies").join(id))
        .unwrap()
        .unwrap()
}

#[test]
fn a_request_fx_0_0_7_never_finished_becomes_the_copys_recovery() {
    let fixture = Fixture::new();
    let output = "x".repeat(6_000);
    let log = LegacyLog::started_007("legacy-crashed")
        .turn(&reply_007("first", "done"))
        .frame("recovery_checkpoint_set", &set(&checkpoint_007(&output)));
    assert_eq!(fixture.summary(&log).unwrap().unwrap().history_len, 1);

    let (events, _) = written(&fixture, &log);
    assert_eq!(events.len(), 3);
    let dir = copy_dir(&fixture, "legacy-crashed");
    let checkpoint = read_checkpoint(&dir, 3).unwrap().unwrap();
    assert_eq!(
        checkpoint.transcript().entries,
        [
            HistoryEntry::User("run the tests".to_owned()),
            HistoryEntry::Assistant("Checking.".to_owned()),
            HistoryEntry::Assistant("Running them now.".to_owned()),
        ]
    );
    let handle = make_handle("call_1", "run_command", &output);
    let stored = fixture
        .root
        .path()
        .join("copies/legacy-crashed/tool-results")
        .join(&handle);
    assert_eq!(fs::read_to_string(stored).unwrap(), output);
    let saved = fs::read_to_string(
        fixture
            .root
            .path()
            .join("copies/legacy-crashed/recovery.json"),
    )
    .unwrap();
    assert!(
        saved.starts_with("{\"conversation_seq\":3,\"checkpoint\":{\"version\":2,"),
        "{saved}"
    );
    assert!(saved.contains("\"schema_version\":10"), "{saved}");
    assert!(
        saved.contains(&format!("\"output\":\"\",\"output_handle\":\"{handle}\"")),
        "{saved}"
    );
    assert!(
        saved.contains(&format!("\"credential_identity\":\"{IDENTITY}\"")),
        "{saved}"
    );
}

#[test]
fn a_checkpoint_from_before_fxs_public_release_is_archived_as_a_failed_turn() {
    let fixture = Fixture::new();
    let legacy = REQUEST_CHECKPOINT.replace(
        "\"assistant_source\":\"\"",
        "\"assistant_source\":\"saved partial\"",
    );
    let log = LegacyLog::started_007("legacy-archived")
        .turn(&reply_007("first", "done"))
        .frame("recovery_checkpoint_set", &legacy);
    assert_eq!(fixture.summary(&log).unwrap().unwrap().history_len, 2);
    let (events, _) = written(&fixture, &log);
    assert_eq!(prompts(&events), ["first", "saved request"]);
    assert_eq!(
        events.last(),
        Some(&ConversationEvent::Interrupted(InterruptedEvent::new(
            InterruptReason::Failed,
            Some("saved partial".to_owned())
        )))
    );
    assert!(
        !fixture
            .root
            .path()
            .join("copies/legacy-archived/recovery.json")
            .exists()
    );
}

#[test]
fn a_version_one_checkpoint_names_its_route_without_a_credential() {
    let fixture = Fixture::new();
    let version_one = checkpoint_007("ok")
        .replace("\"version\":2", "\"version\":1")
        .replace(
            &format!("\"authority\":{{\"provider\":\"gateway\",\"model\":\"openai/gpt-5\",\"credential_source\":\"ai_gateway_api_key\",\"credential_identity\":\"{IDENTITY}\"}}"),
            "\"route_model\":\"openai/gpt-5\",\"route_provider\":\"codex\"",
        );
    let log = LegacyLog::started_007("legacy-version-one")
        .frame("recovery_checkpoint_set", &set(&version_one));
    written(&fixture, &log);
    let saved = fs::read_to_string(
        fixture
            .root
            .path()
            .join("copies/legacy-version-one/recovery.json"),
    )
    .unwrap();
    assert!(
        saved.contains("\"authority\":{\"provider\":\"codex\",\"model\":\"openai/gpt-5\",\"credential_source\":null,\"credential_identity\":null}"),
        "{saved}"
    );
    assert!(
        read_checkpoint(&copy_dir(&fixture, "legacy-version-one"), 0)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_checkpoint_keeps_its_steering_and_turn_summary() {
    let fixture = Fixture::new();
    let later = checkpoint_007("ok").replace(
        "\"schema_version\":4,\"tool_steps\":[{\"assistant\":\"Checking.\",\"tool_calls\":[{\"id\":\"call_1\",\"name\":\"run_command\",\"arguments_json\":\"{\\\"command\\\":\\\"cargo test\\\"}\",\"provider_result\":null}],\"tool_results\":[",
        "\"schema_version\":7,\"tool_steps\":[{\"assistant\":\"Checking.\",\"tool_calls\":[{\"id\":\"call_1\",\"name\":\"run_command\",\"arguments_json\":\"{\\\"command\\\":\\\"cargo test\\\"}\",\"provider_result\":null}],\"tool_results\":[",
    ).replace(
        "\"files\":[]}",
        "\"files\":[],\"steering\":[{\"text\":\"also lint\",\"assistant_prefix\":\"\",\"after_tool_step_count\":1}],\"turn_summary\":{\"started_at_ms\":5,\"completed_at_ms\":9,\"thinking_duration_ms\":1,\"turn_duration_ms\":4,\"token_progress\":{\"input_tokens\":12,\"output_tokens\":3,\"input_exact\":true,\"output_exact\":false}}}",
    );
    assert_ne!(later, checkpoint_007("ok"));
    let log =
        LegacyLog::started_007("legacy-steered").frame("recovery_checkpoint_set", &set(&later));
    written(&fixture, &log);
    let checkpoint = read_checkpoint(&copy_dir(&fixture, "legacy-steered"), 0)
        .unwrap()
        .unwrap();
    let summary = checkpoint.turn_summary().unwrap();
    assert_eq!(
        (
            summary.started_at_ms,
            summary.turn_duration_ms,
            summary.token_progress.input_tokens
        ),
        (5, 4, 12)
    );
    assert!(
        checkpoint
            .transcript()
            .entries
            .contains(&HistoryEntry::User("also lint".to_owned())),
        "{:?}",
        checkpoint.transcript().entries
    );
}

#[test]
fn a_checkpoint_too_large_to_save_is_left_out_as_upstream_leaves_it() {
    let fixture = Fixture::new();
    let control_bytes = 11_250_000;
    let oversized = checkpoint_007("MARK")
        .replace(
            "\"output\":\"MARK\"",
            &format!(
                "\"output\":{{\"encoding\":\"base64\",\"data\":\"{}\"}}",
                "AQEB".repeat(control_bytes / 3)
            ),
        )
        .replace(
            "\"output_bytes\":4,\"stored_output_bytes\":4",
            &format!("\"output_bytes\":{control_bytes},\"stored_output_bytes\":{control_bytes}"),
        );
    let log = LegacyLog::started_007("legacy-oversized")
        .turn(&reply_007("first", "done"))
        .frame("recovery_checkpoint_set", &set(&oversized));
    let (events, _) = written(&fixture, &log);
    assert_eq!(prompts(&events), ["first"]);
    let copy = fixture.root.path().join("copies/legacy-oversized");
    assert!(copy.join("events.jsonl").exists());
    assert!(!copy.join("recovery.json").exists());
}

fn saved_recovery(fixture: &Fixture, id: &str, checkpoint: &str) -> String {
    let log = LegacyLog::started_007(id).frame("recovery_checkpoint_set", &set(checkpoint));
    written(fixture, &log);
    let dir = copy_dir(fixture, id);
    assert!(read_checkpoint(&dir, 0).unwrap().is_some());
    fs::read_to_string(
        fixture
            .root
            .path()
            .join("copies")
            .join(id)
            .join("recovery.json"),
    )
    .unwrap()
}

#[test]
fn a_checkpoint_writes_its_ultra_pair_only_once_either_is_set() {
    let fixture = Fixture::new();
    let with_pair = |requested: bool, effective: bool| {
        checkpoint_007("ok").replace(
            "\"fast_mode\":false,\"max",
            &format!("\"fast_mode\":false,\"requested_ultrafast_mode\":{requested},\"ultrafast_mode\":{effective},\"max"),
        )
    };
    let unset = saved_recovery(&fixture, "legacy-ultra-unset", &with_pair(false, false));
    assert!(!unset.contains("ultrafast"), "{unset}");
    let set_pair = saved_recovery(&fixture, "legacy-ultra-set", &with_pair(true, false));
    assert!(
        set_pair.contains("\"requested_ultrafast_mode\":true,\"ultrafast_mode\":false"),
        "{set_pair}"
    );
}

#[test]
fn a_checkpoint_drops_file_evidence_without_a_path_and_keeps_a_host_managed_credential() {
    let fixture = Fixture::new();
    let evidence = |path: &str| {
        format!(
            "{{\"path\":\"{path}\",\"new_path\":null,\"tool_call_id\":\"call_1\",\"tool_name\":\"read_file\",\"action\":\"read\",\"status\":\"success\",\"model_view_covers_full_file\":false,\"stale\":false}}"
        )
    };
    let checkpoint = checkpoint_007("ok")
        .replace(
            "\"files\":[]",
            &format!("\"files\":[{},{}]", evidence(""), evidence("src/main.rs")),
        )
        .replace("\"provider\":\"gateway\"", "\"provider\":\"codex\"")
        .replace("\"ai_gateway_api_key\"", "\"host_managed\"");
    let saved = saved_recovery(&fixture, "legacy-evidence", &checkpoint);
    assert!(saved.contains("\"path\":\"src/main.rs\""), "{saved}");
    assert!(!saved.contains("\"path\":\"\""), "{saved}");
    assert!(
        saved.contains("\"provider\":\"codex\",\"model\":\"openai/gpt-5\",\"credential_source\":\"host_managed\""),
        "{saved}"
    );
}

#[test]
fn a_replaced_state_can_carry_the_recovery_it_left_open() {
    let fixture = Fixture::new();
    let state = state_007(
        "legacy-replaced-open",
        50,
        &[reply_007("first", "done")],
        0,
        &format!(",\"recovery_checkpoint\":{}", checkpoint_007("ok")),
    );
    let log = LegacyLog::started_007("legacy-replaced-open")
        .turn(&reply_007("first", "done"))
        .replaced("compaction", &state, 50);
    written(&fixture, &log);
    assert!(
        read_checkpoint(&copy_dir(&fixture, "legacy-replaced-open"), 3)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_checkpoint_upstream_would_refuse_or_oh_fx_cannot_keep_hides_the_session() {
    let fixture = Fixture::new();
    let base = checkpoint_007("ok");
    let cases = [
        ("turn id", base.replace("\"turn_id\":2", "\"turn_id\":0")),
        (
            "attempts",
            base.replace(
                "\"consumed_provider_attempts\":1",
                "\"consumed_provider_attempts\":4",
            ),
        ),
        (
            "reservation",
            base.replace(
                "\"consumed_provider_attempts\":1",
                "\"consumed_provider_attempts\":3",
            )
            .replace(
                "\"outstanding_reservation\":false",
                "\"outstanding_reservation\":true",
            ),
        ),
        (
            "identity without source",
            base.replace(
                "\"credential_source\":\"ai_gateway_api_key\"",
                "\"credential_source\":null",
            ),
        ),
        ("cause", base.replace("network_interrupted", "solar_flare")),
        ("version", base.replace("\"version\":2", "\"version\":3")),
        (
            "extra key",
            base.replace(
                "\"turn_id\":2,",
                "\"turn_id\":2,\"delivery\":\"possibly_sent\",",
            ),
        ),
        (
            "work id",
            base.replace(
                "\"text\":\"run the tests\",\"images\":[]",
                "\"text\":\"run the tests\",\"images\":[],\"work_id\":\"work-1\"",
            ),
        ),
        (
            "half an ultra request",
            base.replace(
                "\"fast_mode\":false,\"max",
                "\"fast_mode\":false,\"ultrafast_mode\":false,\"max",
            ),
        ),
        (
            "empty model",
            base.replace("\"model\":\"openai/gpt-5\"", "\"model\":\"\""),
        ),
        (
            "padded model",
            base.replace("\"model\":\"openai/gpt-5\"", "\"model\":\"openai/gpt-5 \""),
        ),
        (
            "credential of another provider",
            base.replace("\"provider\":\"gateway\"", "\"provider\":\"codex\""),
        ),
        (
            "padded legacy model",
            REQUEST_CHECKPOINT[14..REQUEST_CHECKPOINT.len() - 1]
                .replace("\"route_model\":\"", "\"route_model\":\" "),
        ),
        (
            "legacy route",
            REQUEST_CHECKPOINT[14..REQUEST_CHECKPOINT.len() - 1]
                .replace("vercel_ai_gateway", "elsewhere"),
        ),
    ];
    for (case, checkpoint) in cases {
        let log = LegacyLog::started_007("legacy-refused")
            .frame("recovery_checkpoint_set", &set(&checkpoint));
        assert!(fixture.summary(&log).is_err(), "{case}");
    }
}
