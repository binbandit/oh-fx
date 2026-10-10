use super::replacements::{reply_007, state_007, written};
use super::*;
use crate::session_usage_sidecar::{SIDECAR_FILE, load_conversation};

const SAVED: &str = "{\"billing\":\"complete\",\"api_duration_complete\":true,\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":2,\"settled_through_sequence\":1,\"api_duration_ms\":10,\"wall_duration_ms\":20,\"total_cost\":1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":2,\"cache_write_tokens\":0,\"billable_web_search_calls\":0,\"lines_added\":4,\"lines_removed\":1,\"models\":[{\"model\":\"test/model\",\"first_sequence\":1,\"total_cost\":1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":2,\"cache_write_tokens\":0,\"billable_web_search_calls\":0}],\"pending\":[]}";

fn sidecar(fixture: &Fixture, log: &LegacyLog) -> Option<String> {
    written(fixture, log);
    let copy = fixture.root.path().join("copies").join(&log.id);
    fs::read_to_string(copy.join(SIDECAR_FILE)).ok()
}

fn rich(id: &str, billing: &str, snapshot: &str) -> String {
    format!(
        "{{\"schema_version\":1,\"session_id\":\"{id}\",\"snapshot\":{{\"schema_version\":3,\"billing\":\"{billing}\",{snapshot}}}}}"
    )
}

#[test]
fn the_converted_copy_saves_the_usage_the_session_checkpointed_last() {
    let fixture = Fixture::new();
    let log = LegacyLog::started("legacy-usage", "/work")
        .turn(&reply("first", "answer"))
        .frame("usage_checkpointed", &format!("{{\"usage\":{SAVED}}}"));
    let saved = sidecar(&fixture, &log).unwrap();
    assert_eq!(
        saved,
        rich(
            "legacy-usage",
            "complete",
            "\"api_duration_complete\":true,\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":2,\"settled_through_sequence\":1,\"api_duration_ms\":10,\"wall_duration_ms\":20,\"total_cost\":1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":2,\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"request_count\":null,\"billable_web_search_calls\":0,\"lines_added\":4,\"lines_removed\":1,\"models\":[{\"model\":\"test/model\",\"first_sequence\":1,\"total_cost\":1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":2,\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"request_count\":null,\"billable_web_search_calls\":0}],\"pending\":[],\"publication_backlog\":[],\"incidents\":[]"
        )
    );
    let copy = PrivateDir::open_existing(&fixture.root.path().join("copies/legacy-usage"))
        .unwrap()
        .unwrap();
    let restored = load_conversation(&copy, "legacy-usage", 0).unwrap();
    assert!(restored.incidents.is_empty());
    assert_eq!(restored.input_tokens, 10);
}

#[test]
fn a_replaced_state_carries_its_own_usage() {
    let fixture = Fixture::new();
    let state = state_007(
        "legacy-replaced-usage",
        77,
        &[reply_007("one", "first")],
        0,
        &format!(",\"usage\":{SAVED}"),
    );
    let log = LegacyLog::started_007("legacy-replaced-usage")
        .turn(&reply_007("one", "first"))
        .replaced("compaction", &state, 77);
    let saved = sidecar(&fixture, &log).unwrap();
    assert!(saved.contains("\"input_tokens\":10,"), "{saved}");

    let without = state_007("legacy-no-usage", 77, &[reply_007("one", "first")], 0, "");
    let log = LegacyLog::started_007("legacy-no-usage")
        .turn(&reply_007("one", "first"))
        .replaced("compaction", &without, 77);
    assert_eq!(sidecar(&fixture, &log), None);
}

#[test]
fn separate_cache_accounting_from_older_fx_becomes_legacy_usage() {
    let fixture = Fixture::new();
    let separate = SAVED.replacen("\"cache_read_tokens\":2", "\"cache_read_tokens\":11", 2);
    let log = LegacyLog::started("legacy-cache", "/work")
        .turn(&reply("first", "answer"))
        .frame("usage_checkpointed", &format!("{{\"usage\":{separate}}}"));
    assert_eq!(
        sidecar(&fixture, &log).unwrap(),
        rich(
            "legacy-cache",
            "legacy",
            "\"api_duration_complete\":false,\"wall_duration_complete\":false,\"code_complete\":false,\"next_sequence\":1,\"settled_through_sequence\":0,\"api_duration_ms\":0,\"wall_duration_ms\":0,\"total_cost\":0,\"input_tokens\":0,\"output_tokens\":0,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"request_count\":null,\"billable_web_search_calls\":0,\"lines_added\":0,\"lines_removed\":0,\"models\":[],\"pending\":[],\"publication_backlog\":[],\"incidents\":[]"
        )
    );
}

#[test]
fn malformed_saved_usage_makes_the_session_unreadable() {
    let fixture = Fixture::new();
    for broken in [
        SAVED.replacen("\"input_tokens\":10", "\"input_tokens\":9", 1),
        SAVED.replacen("\"next_sequence\":2", "\"next_sequence\":0", 1),
        SAVED.replacen("\"total_cost\":1", "\"total_cost\":-1", 1),
        SAVED.replacen("\"first_sequence\":1", "\"first_sequence\":0", 1),
        SAVED.replacen("\"pending\":[]", "\"pending\":[],\"unknown\":null", 1),
        "null".to_owned(),
    ] {
        let log = LegacyLog::started("legacy-broken-usage", "/work")
            .turn(&reply("first", "answer"))
            .frame("usage_checkpointed", &format!("{{\"usage\":{broken}}}"));
        assert!(fixture.summary(&log).is_err(), "{broken}");
    }
}
