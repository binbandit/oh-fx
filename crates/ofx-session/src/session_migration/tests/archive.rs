use super::replacements::{compacted_007, reply_007};
use super::*;
use crate::session_log::{
    ArchivedTurn, CompactedHistory, ExecutedStep, SessionArchive, TurnExecution,
};
use crate::session_summary_codec::SessionSource;

impl Fixture {
    pub(super) fn archive(&self, log: &LegacyLog) -> Result<SessionArchive, SessionError> {
        archive_schema_v3(&self.dir(log), &log.id)
    }
}

#[test]
fn a_saved_turn_shows_its_tool_output_inline_as_upstream_reads_it() {
    let fixture = Fixture::new();
    let log = LegacyLog::started("legacy-detail", "/work").turn(&command_turn(
        "list",
        "call_1",
        &command_result("call_1", "listing", "null"),
        "done",
    ));
    let archive = fixture.archive(&log).unwrap();
    assert_eq!(
        (
            archive.id.as_str(),
            archive.created_at_ms,
            archive.conversation_language.as_str(),
            archive.source,
        ),
        ("legacy-detail", 10, "en", SessionSource::OhFx)
    );
    let [
        ArchivedTurn::Replied {
            user,
            assistant,
            execution,
        },
    ] = archive.turns.as_slice()
    else {
        panic!("{:?}", archive.turns);
    };
    assert_eq!((user.as_str(), assistant.as_str()), ("list", "done"));
    assert_eq!(
        execution.presentation_json().to_string(),
        "{\"schema_version\":3,\"tool_steps\":[{\"assistant\":\"Checking.\",\"tool_calls\":[{\"id\":\"call_1\",\"name\":\"run_command\",\"arguments_json\":\"{\\\"command\\\":\\\"ls\\\"}\",\"provider_result\":null}],\"tool_results\":[{\"tool_call_id\":\"call_1\",\"tool_name\":\"run_command\",\"status\":\"success\",\"output\":\"listing\",\"output_bytes\":7,\"stored_output_bytes\":7,\"truncated\":false,\"provider_native\":false,\"created_at_ms\":15,\"permission_feedback\":[]}]}],\"files\":[{\"path\":\"src/main.rs\",\"new_path\":null,\"tool_call_id\":\"call_1\",\"tool_name\":\"read_file\",\"action\":\"read\",\"status\":\"success\",\"model_view_covers_full_file\":true,\"stale\":false},{\"path\":\"\",\"new_path\":null,\"tool_call_id\":\"call_1\",\"tool_name\":\"read_file\",\"action\":\"read\",\"status\":\"success\",\"model_view_covers_full_file\":false,\"stale\":false}],\"steering\":[]}"
    );
}

#[test]
fn an_interrupted_turn_keeps_its_call_and_the_tools_that_finished() {
    let fixture = Fixture::new();
    let interrupted = "{\"kind\":\"interrupted\",\"user\":{\"text\":\"stop me\",\"images\":[]},\"assistant\":\"partial\",\"tool_call\":{\"id\":\"call_9\",\"name\":\"run_command\",\"arguments_json\":\"[1]\",\"provider_result\":null},\"completed_tool_names\":[\"read_file\",\"list_files\"],\"terminal_reason\":\"failed\",\"execution\":{\"schema_version\":3,\"tool_steps\":[{\"assistant\":null,\"tool_calls\":[],\"tool_results\":[]}],\"files\":[]}}";
    let log = LegacyLog::started("legacy-stopped", "/work").turn(interrupted);
    assert_eq!(
        fixture.archive(&log).unwrap().turns,
        [ArchivedTurn::Interrupted {
            user: "stop me".to_owned(),
            assistant: Some("partial".to_owned()),
            tool_call: Some(ToolCallEvent::new(
                "call_9",
                "run_command",
                "{}",
                ToolArgumentIntegrity::Valid
            )),
            completed_tool_names: vec!["read_file".to_owned(), "list_files".to_owned()],
            execution: TurnExecution {
                steps: vec![ExecutedStep {
                    assistant: None,
                    calls: Vec::new(),
                    results: Vec::new(),
                }],
                ..TurnExecution::default()
            },
        }]
    );
}

#[test]
fn compactions_keep_their_place_and_count() {
    let fixture = Fixture::new();
    let log = LegacyLog::started_007("legacy-compactions")
        .turn(&reply_007("one", "first"))
        .turn(&compacted_007("Earlier: one.", 1, 1))
        .turn(&reply_007("two", "second"))
        .turn(&compacted_007("Earlier: one and two.", 2, 2))
        .turn(&reply_007("three", "third"));
    let turns = fixture.archive(&log).unwrap().turns;
    assert_eq!(turns.len(), 5);
    assert_eq!(
        turns[3],
        ArchivedTurn::Compacted(CompactedHistory {
            summary: "Earlier: one and two.".to_owned(),
            removed_turn_count: 2,
            compaction_count: 2,
        })
    );
}

#[test]
fn a_subagent_child_is_not_shown() {
    let fixture = Fixture::new();
    let log = LegacyLog::started("legacy-child", "/work").turn(&reply("one", "two"));
    let child = LegacyLog {
        frames: log
            .frames
            .into_iter()
            .map(|(kind, payload, timestamp_ms)| {
                (
                    kind,
                    payload.replace(
                        &format!(",\"usage\":{USAGE}}}"),
                        &format!(",\"usage\":{USAGE},\"subagent_child\":true}}"),
                    ),
                    timestamp_ms,
                )
            })
            .collect(),
        ..log
    };
    assert_eq!(fixture.archive(&child), Err(SessionError::SessionNotFound));
}

#[test]
fn only_the_prefix_the_watermark_commits_is_shown() {
    let fixture = Fixture::new();
    let log = LegacyLog::started("legacy-prefix", "/work")
        .turn(&reply("kept", "answer"))
        .turn(&reply("never acknowledged", "lost"))
        .committed_through(3)
        .tail("{\"torn");
    let archive = fixture.archive(&log).unwrap();
    assert_eq!(archive.updated_at_ms, 30);
    assert!(matches!(
        archive.turns.as_slice(),
        [ArchivedTurn::Replied { user, .. }] if user == "kept"
    ));

    let dir = fixture.dir(&log);
    fs::write(
        fixture
            .root
            .path()
            .join("legacy-prefix")
            .join(format!("commit.{GENERATION}.json")),
        log.watermark().replace("legacy-prefix", "other-session"),
    )
    .unwrap();
    assert_eq!(
        archive_schema_v3(&dir, "legacy-prefix"),
        Err(SessionError::InvalidSessionFormat)
    );
}
