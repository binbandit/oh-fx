use ofx_contract::{ChatMessage, ToolCallId};

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
