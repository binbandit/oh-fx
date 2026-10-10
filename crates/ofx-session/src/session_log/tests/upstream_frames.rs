use ofx_contract::{ChatMessage, FileChangeStats, SavedFileChange, ToolCallId};

use super::*;
use crate::history_snapshot::{HISTORY_CACHE_FILE, HistoryCache};
use crate::session_event::encode_conversation_frame;

const REPLAY: &str =
    "fx-command-replay-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.bin";

fn upstream_log(events: &[String]) -> String {
    events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            format!(
                "{{\"schema_version\":3,\"seq\":{},\"timestamp_ms\":5,\"event\":{event}}}\n",
                index + 1
            )
        })
        .collect::<Vec<_>>()
        .concat()
}

fn upstream_session(fixture: &Fixture, id: &str, events: &[String]) -> String {
    drop(fixture.start(id));
    let log = upstream_log(events);
    fixture.append_raw(id, log.as_bytes());
    log
}

fn reencoded(history: &SavedHistory) -> String {
    let events = history.turns.iter().flat_map(|turn| &turn.events);
    events
        .enumerate()
        .map(|(index, event)| {
            let seq = u64::try_from(index + 1).unwrap();
            String::from_utf8(encode_conversation_frame(seq, 5, event).unwrap()).unwrap()
        })
        .collect()
}

fn cache_coverage(fixture: &Fixture, id: &str) -> Option<u64> {
    let dir = fixture.sessions.open_child(id).unwrap().unwrap();
    let log = File::open(fixture.events(id)).unwrap();
    let length = log.metadata().unwrap().len();
    HistoryCache::open(&dir, id, &log, length).map(|cache| cache.covered())
}

fn shell_call(id: &str, command: &str) -> String {
    format!(
        "{{\"tool_call\":{{\"call_id\":\"{id}\",\"tool_name\":\"shell\",\"arguments_json\":\"{{\\\"command\\\":\\\"{command}\\\"}}\",\"argument_integrity\":\"valid\",\"provisional_id\":null,\"provider_result\":null,\"final_identity\":\"valid\",\"provenance\":\"fx_local\"}}}}"
    )
}

fn command_replay_turns() -> Vec<String> {
    vec![
        "{\"user\":{\"text\":\"list the files\",\"images\":[],\"work_id\":null}}".to_owned(),
        "{\"assistant\":{\"text\":\"\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned(),
        shell_call("call-ls", "ls"),
        format!(
            "{{\"tool_result\":{{\"call_id\":\"call-ls\",\"tool_name\":\"shell\",\"status\":\"success\",\"artifact_ref\":\"{REPLAY}\",\"tool_image_handle\":null,\"output_bytes\":12,\"stored_bytes\":12,\"completeness\":\"complete\",\"preview\":\"a.txt\\nb.txt\\n\",\"provider_native\":false,\"created_at_ms\":5,\"permission_feedback\":[],\"committed_file_presentation\":null,\"command_replay_ref\":\"{REPLAY}\",\"command_replay_bytes\":29,\"command_process_presentation\":{{\"exit_code\":0}},\"terminal_action_presentation\":null}}}}"
        ),
        "{\"assistant\":{\"text\":\"Two files.\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned(),
        "{\"turn_completed\":{\"files\":[],\"turn_summary\":null}}".to_owned(),
        "{\"user\":{\"text\":\"run the tests\",\"images\":[],\"work_id\":null}}".to_owned(),
        shell_call("call-test", "cargo test"),
        format!(
            "{{\"interrupted\":{{\"reason\":\"cancelled\",\"partial_text\":\"Running the tests.\",\"command_replay_ref\":\"{REPLAY}\",\"command_replay_bytes\":29,\"command_artifact_ref\":\"fx-command-artifact-1.log\",\"files\":[],\"turn_summary\":null}}}}"
        ),
    ]
}

fn replay_sidecar() -> Vec<u8> {
    let payload = b"a.txt\nb.txt\n";
    let mut bytes = b"FXRPLY01".to_vec();
    bytes.push(0);
    bytes.extend(u64::try_from(payload.len()).unwrap().to_le_bytes());
    bytes.extend(payload);
    bytes
}

#[test]
fn a_session_holding_upstream_command_replays_resumes_and_keeps_them() {
    let fixture = Fixture::new();
    let id = "fx-command-replays";
    let log = upstream_session(&fixture, id, &command_replay_turns());
    let commands = fixture.dir(id).join("logs/commands");
    fs::create_dir_all(&commands).unwrap();
    fs::write(commands.join(REPLAY), replay_sidecar()).unwrap();

    let mut session = fixture.resume(id).unwrap();
    let history = session.take_history();
    drop(session);
    assert_eq!(reencoded(&history), log);
    assert_eq!(fs::read_to_string(fixture.events(id)).unwrap(), log);
    assert!(fixture.dir(id).join(HISTORY_CACHE_FILE).exists());
    assert_eq!(
        cache_coverage(&fixture, id),
        Some(u64::try_from(log.len()).unwrap())
    );

    let edited = log.replacen("\"preview\":\"a.txt", "\"preview\":\"A.txt", 1);
    fs::write(fixture.events(id), &edited).unwrap();
    let mut cached = fixture.resume(id).unwrap();
    assert_eq!(cached.take_history(), history);
    drop(cached);
    fs::write(fixture.events(id), &log).unwrap();
    assert_eq!(
        fs::read(commands.join(REPLAY)).unwrap(),
        replay_sidecar(),
        "oh-fx leaves the replay sidecar as fx wrote it"
    );

    let restored = fixture.resume(id).unwrap().restored_history().unwrap();
    let tool_results: Vec<_> = restored
        .messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Tool {
                call_id, content, ..
            } => Some((call_id.clone(), content.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_results,
        [
            (ToolCallId::new("call-ls"), "a.txt\nb.txt\n"),
            (ToolCallId::new("call-test"), "aborted by user"),
        ]
    );
}

const DIFF_PACK: &str = "diff-0011223344556677-8899aabbccddeeff.json";

fn file_call(id: &str, tool: &str) -> String {
    format!(
        "{{\"tool_call\":{{\"call_id\":\"{id}\",\"tool_name\":\"{tool}\",\"arguments_json\":\"{{\\\"path\\\":\\\"src/lib.rs\\\"}}\",\"argument_integrity\":\"valid\",\"provisional_id\":null,\"provider_result\":null,\"final_identity\":\"valid\",\"provenance\":\"fx_local\"}}}}"
    )
}

fn file_result(id: &str, tool: &str, presentation: &str) -> String {
    format!(
        "{{\"tool_result\":{{\"call_id\":\"{id}\",\"tool_name\":\"{tool}\",\"status\":\"success\",\"artifact_ref\":\"result-{id}.txt\",\"tool_image_handle\":null,\"output_bytes\":4,\"stored_bytes\":4,\"completeness\":\"complete\",\"preview\":\"done\",\"provider_native\":false,\"created_at_ms\":5,\"permission_feedback\":[],\"committed_file_presentation\":{presentation},\"command_replay_ref\":null,\"command_replay_bytes\":null,\"command_process_presentation\":null,\"terminal_action_presentation\":null}}}}"
    )
}

fn file_presentation_turn() -> Vec<String> {
    let edited = format!(
        "{{\"path\":\"src/lib.rs\",\"kind\":\"edited\",\"lines\":[{{\"kind\":\"deletion\",\"old_line\":2,\"new_line\":null,\"text\":\"    old();\"}},{{\"kind\":\"addition\",\"old_line\":null,\"new_line\":2,\"text\":\"    new();\"}},{{\"kind\":\"addition\",\"old_line\":null,\"new_line\":3,\"text\":\"    more();\"}}],\"additions\":2,\"deletions\":1,\"truncated\":false,\"previous_content\":null,\"after_content\":null,\"lifecycle_id\":{{\"turn_id\":1,\"call_id\":\"call-edit\"}},\"content_handle\":\"{DIFF_PACK}\"}}"
    );
    let added = "{\"path\":\"notes.md\",\"kind\":\"added\",\"lines\":[{\"kind\":\"addition\",\"old_line\":null,\"new_line\":1,\"text\":\"# Notes\"}],\"additions\":1,\"deletions\":0,\"truncated\":false,\"previous_content\":null,\"after_content\":\"# Notes\\n\",\"lifecycle_id\":{\"turn_id\":1,\"call_id\":\"call-write\"},\"content_handle\":null}";
    vec![
        "{\"user\":{\"text\":\"edit the code\",\"images\":[],\"work_id\":null}}".to_owned(),
        "{\"assistant\":{\"text\":\"\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned(),
        file_call("call-edit", "edit_file"),
        file_call("call-write", "write_file"),
        file_result("call-edit", "edit_file", &edited),
        file_result("call-write", "write_file", added),
        "{\"assistant\":{\"text\":\"Edited.\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned(),
        "{\"turn_completed\":{\"files\":[],\"turn_summary\":null}}".to_owned(),
    ]
}

#[test]
fn a_session_holding_upstream_file_presentations_resumes_and_keeps_them() {
    let fixture = Fixture::new();
    let id = "fx-file-presentations";
    let log = upstream_session(&fixture, id, &file_presentation_turn());
    let results = fixture.dir(id).join("tool-results");
    fs::create_dir_all(&results).unwrap();
    let pack = "{\"previous_content\":\"old\\n\",\"after_content\":\"new\\n\"}";
    fs::write(results.join(DIFF_PACK), pack).unwrap();

    let mut session = fixture.resume(id).unwrap();
    let history = session.take_history();
    drop(session);
    assert_eq!(reencoded(&history), log);
    assert_eq!(fs::read_to_string(fixture.events(id)).unwrap(), log);
    let changes: Vec<_> = history.turns[0]
        .events
        .iter()
        .filter_map(|event| match event {
            ConversationEvent::ToolResult(result) => Some(result.file_change()),
            _ => None,
        })
        .collect();
    let saved = |path: &str, additions, deletions| SavedFileChange {
        path: path.to_owned(),
        stats: FileChangeStats::from_lines(additions, deletions),
    };
    assert_eq!(
        changes,
        [
            Some(saved("src/lib.rs", 2, 1)),
            Some(saved("notes.md", 1, 0))
        ]
    );

    let edited = log.replacen(
        "\"text\":\"edit the code\"",
        "\"text\":\"Edit the code\"",
        1,
    );
    fs::write(fixture.events(id), &edited).unwrap();
    let mut cached = fixture.resume(id).unwrap();
    assert_eq!(cached.take_history(), history);
    drop(cached);
    fs::write(fixture.events(id), &log).unwrap();
    assert_eq!(
        fs::read_to_string(results.join(DIFF_PACK)).unwrap(),
        pack,
        "oh-fx leaves the diff pack as fx wrote it"
    );

    let restored = fixture.resume(id).unwrap().restored_history().unwrap();
    let contents: Vec<_> = restored
        .messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Tool { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(contents, ["done", "done"]);
}

const HELD: &str = "Security review held this action.";

fn review_hold_turn() -> Vec<String> {
    vec![
        "{\"user\":{\"text\":\"clean the build\",\"images\":[],\"work_id\":null}}".to_owned(),
        "{\"assistant\":{\"text\":\"\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned(),
        shell_call("call-rm", "rm -rf build"),
        format!(
            "{{\"tool_result\":{{\"call_id\":\"call-rm\",\"tool_name\":\"shell\",\"status\":\"failure\",\"artifact_ref\":\"result-call-rm.txt\",\"tool_image_handle\":null,\"output_bytes\":33,\"stored_bytes\":33,\"completeness\":\"complete\",\"preview\":\"{HELD}\",\"provider_native\":false,\"review_feedback\":true,\"created_at_ms\":5,\"permission_feedback\":[],\"committed_file_presentation\":null,\"command_replay_ref\":null,\"command_replay_bytes\":null,\"command_process_presentation\":null,\"terminal_action_presentation\":null}}}}"
        ),
        "{\"assistant\":{\"text\":\"The review held it.\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned(),
        "{\"turn_completed\":{\"files\":[],\"turn_summary\":null}}".to_owned(),
    ]
}

#[test]
fn a_session_holding_upstream_review_feedback_resumes_and_keeps_it() {
    let fixture = Fixture::new();
    let id = "fx-review-feedback";
    let log = upstream_session(&fixture, id, &review_hold_turn());

    let mut session = fixture.resume(id).unwrap();
    let history = session.take_history();
    drop(session);
    assert_eq!(reencoded(&history), log);
    assert_eq!(fs::read_to_string(fixture.events(id)).unwrap(), log);
    assert_eq!(
        cache_coverage(&fixture, id),
        Some(u64::try_from(log.len()).unwrap())
    );

    let edited = log.replacen(
        "\"text\":\"clean the build\"",
        "\"text\":\"Clean the build\"",
        1,
    );
    fs::write(fixture.events(id), &edited).unwrap();
    let mut cached = fixture.resume(id).unwrap();
    assert_eq!(cached.take_history(), history);
    drop(cached);
    fs::write(fixture.events(id), &log).unwrap();

    let restored = fixture.resume(id).unwrap().restored_history().unwrap();
    assert!(
        restored.messages.contains(&ChatMessage::Tool {
            call_id: ToolCallId::new("call-rm"),
            tool_name: "shell".to_owned(),
            content: HELD.to_owned(),
            status: ToolResultStatus::Failure,
        }),
        "{:?}",
        restored.messages
    );
}
