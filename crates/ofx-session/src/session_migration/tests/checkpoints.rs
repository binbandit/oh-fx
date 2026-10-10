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
