use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

use super::replacements::{reply_007, written};
use super::*;
use crate::session_event::{
    CommittedFilePresentation, LifecycleId, PresentationLine, ToolResultEvent, WireTag,
};

const EDIT_CALL: &str = "call_edit";

fn result_007(call_id: &str, tool: &str, output: &str, presentation: &str, replay: &str) -> String {
    format!(
        "{{\"tool_call_id\":\"{call_id}\",\"tool_name\":\"{tool}\",\"status\":\"success\",\"output\":{output},\"output_handle\":null,\"preview\":null,\"output_bytes\":12,\"stored_output_bytes\":12,\"truncated\":false,\"provider_native\":false,\"created_at_ms\":15,\"permission_feedback\":[],\"committed_file_presentation\":{presentation},\"command_output_replay\":{replay},\"command_process_presentation\":null,\"terminal_action_presentation\":null}}"
    )
}

fn turn_007(call_id: &str, tool: &str, arguments: &str, result: &str) -> String {
    format!(
        "{{\"kind\":\"assistant\",\"user\":{{\"text\":\"change it\",\"images\":[]}},\"assistant\":\"done\",\"execution\":{{\"schema_version\":4,\"tool_steps\":[{{\"assistant\":\"Working.\",\"tool_calls\":[{{\"id\":\"{call_id}\",\"name\":\"{tool}\",\"arguments_json\":{},\"provider_result\":null}}],\"tool_results\":[{result}]}}],\"files\":[]}}}}",
        serde_json::to_string(arguments).unwrap()
    )
}

fn presentation_007(previous: &str, after: &str) -> String {
    format!(
        "{{\"path\":\"src/main.rs\",\"kind\":\"edited\",\"lines\":[{{\"kind\":\"context\",\"old_line\":1,\"new_line\":1,\"text\":\"fn main() {{\"}},{{\"kind\":\"deletion\",\"old_line\":2,\"new_line\":null,\"text\":\"    old();\"}},{{\"kind\":\"addition\",\"old_line\":null,\"new_line\":2,\"text\":\"    new();\"}}],\"additions\":1,\"deletions\":1,\"truncated\":false,\"previous_content\":{},\"after_content\":{},\"lifecycle_id\":{{\"turn_id\":3,\"call_id\":\"{EDIT_CALL}\"}}}}",
        serde_json::to_string(previous).unwrap(),
        serde_json::to_string(after).unwrap()
    )
}

fn edit_turn_007(presentation: &str) -> String {
    turn_007(
        EDIT_CALL,
        "edit_file",
        "{\"path\":\"src/main.rs\"}",
        &result_007(
            EDIT_CALL,
            "edit_file",
            "\"edited src/main.rs\"",
            presentation,
            "null",
        ),
    )
}

fn expected_presentation(
    previous: Option<&str>,
    after: Option<&str>,
    content_handle: Option<String>,
) -> CommittedFilePresentation {
    let line = |kind, old_line, new_line, text: &str| PresentationLine {
        kind,
        old_line,
        new_line,
        text: text.to_owned(),
    };
    CommittedFilePresentation {
        path: "src/main.rs".to_owned(),
        kind: tag("edited"),
        lines: vec![
            line(tag("context"), Some(1), Some(1), "fn main() {"),
            line(tag("deletion"), Some(2), None, "    old();"),
            line(tag("addition"), None, Some(2), "    new();"),
        ],
        additions: 1,
        deletions: 1,
        truncated: false,
        previous_content: previous.map(str::to_owned),
        after_content: after.map(str::to_owned),
        lifecycle_id: Some(LifecycleId {
            turn_id: 3,
            call_id: EDIT_CALL.to_owned(),
        }),
        content_handle,
    }
}

fn tag<T: WireTag>(name: &str) -> T {
    T::from_tag(name).unwrap()
}

fn results(events: &[ConversationEvent]) -> Vec<&ToolResultEvent> {
    events
        .iter()
        .filter_map(|event| match event {
            ConversationEvent::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

fn digest_hex(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes)[..8])
}

fn tool_results_file(fixture: &Fixture, id: &str, handle: &str) -> Vec<u8> {
    fs::read(
        fixture
            .root
            .path()
            .join("copies")
            .join(id)
            .join("tool-results")
            .join(handle),
    )
    .unwrap()
}

#[test]
fn a_small_edit_keeps_its_presentation_inline() {
    let fixture = Fixture::new();
    let previous = "fn main() {\n    old();\n}\n";
    let after = "fn main() {\n    new();\n}\n";
    let log = LegacyLog::started_007("legacy-edit")
        .turn(&edit_turn_007(&presentation_007(previous, after)));
    let (events, _) = written(&fixture, &log);
    let result = results(&events)[0];
    assert_eq!(
        result.committed_file_presentation.as_deref(),
        Some(&expected_presentation(Some(previous), Some(after), None))
    );
    let change = result.file_change().unwrap();
    assert_eq!(change.path, "src/main.rs");
}

#[test]
fn a_large_edit_moves_its_contents_into_a_diff_pack_as_upstream_names_it() {
    let fixture = Fixture::new();
    let previous = "a\n".repeat(3_000);
    let after = format!("{previous}x\u{1}\"y\t");
    let log = LegacyLog::started_007("legacy-large-edit")
        .turn(&edit_turn_007(&presentation_007(&previous, &after)));
    let (events, _) = written(&fixture, &log);
    let pack = format!(
        "{{\"previous_content\":\"{}\",\"after_content\":\"{}x\\u0001\\\"y\\t\"}}",
        "a\\n".repeat(3_000),
        "a\\n".repeat(3_000)
    );
    let handle = format!(
        "diff-{}-{}.json",
        digest_hex(EDIT_CALL.as_bytes()),
        digest_hex(pack.as_bytes())
    );
    assert_eq!(
        results(&events)[0].committed_file_presentation.as_deref(),
        Some(&expected_presentation(None, None, Some(handle.clone())))
    );
    assert_eq!(
        tool_results_file(&fixture, "legacy-large-edit", &handle),
        pack.into_bytes()
    );
}

#[test]
fn an_edit_whose_contents_were_already_moved_keeps_its_handle() {
    let fixture = Fixture::new();
    let handle = "diff-0011223344556677-8899aabbccddeeff.json";
    let presentation = presentation_007("x", "y")
        .replace("\"previous_content\":\"x\"", "\"previous_content\":null")
        .replace("\"after_content\":\"y\"", "\"after_content\":null")
        .replace(
            &format!("\"call_id\":\"{EDIT_CALL}\"}}}}"),
            &format!("\"call_id\":\"{EDIT_CALL}\"}},\"content_handle\":\"{handle}\"}}"),
        );
    let log = LegacyLog::started_007("legacy-moved-edit").turn(&edit_turn_007(&presentation));
    let (events, _) = written(&fixture, &log);
    assert_eq!(
        results(&events)[0].committed_file_presentation.as_deref(),
        Some(&expected_presentation(None, None, Some(handle.to_owned())))
    );
}

#[test]
fn a_command_replay_is_kept_when_fx_saved_one() {
    let fixture = Fixture::new();
    let available =
        "{\"kind\":\"available\",\"handle\":\"fx-command-replay-0a0b.bin\",\"framed_bytes\":24}";
    let log = LegacyLog::started_007("legacy-replay")
        .turn(&turn_007(
            "call_1",
            "run_command",
            "{\"command\":\"ls\"}",
            &result_007("call_1", "run_command", "\"a b\"", "null", available),
        ))
        .turn(&turn_007(
            "call_2",
            "run_command",
            "{\"command\":\"ls\"}",
            &result_007(
                "call_2",
                "run_command",
                "\"a b\"",
                "null",
                "{\"kind\":\"unavailable\"}",
            ),
        ));
    let (events, _) = written(&fixture, &log);
    let saved: Vec<_> = results(&events)
        .iter()
        .map(|result| {
            (
                result.command_replay_ref.clone(),
                result.command_replay_bytes,
            )
        })
        .collect();
    assert_eq!(
        saved,
        [
            (Some("fx-command-replay-0a0b.bin".to_owned()), Some(24)),
            (None, None)
        ]
    );
}

fn cancelled_turn_007(reason: &str, call: &str) -> String {
    format!(
        "{{\"kind\":\"interrupted\",\"user\":{{\"text\":\"run it\",\"images\":[]}},\"assistant\":null,\"tool_call\":{call},\"completed_tool_names\":[],\"terminal_reason\":\"{reason}\",\"cancelled_command\":{{\"output_replay\":{{\"kind\":\"available\",\"handle\":\"fx-command-replay-0c0d.bin\",\"framed_bytes\":40}},\"command_artifact_handle\":\"result-run_command-0011-2233.txt\"}}}}"
    )
}

const RUN_CALL: &str = "{\"id\":\"call_run\",\"name\":\"run_command\",\"arguments_json\":\"{\\\"command\\\":\\\"sleep 9\\\"}\",\"provider_result\":null}";

#[test]
fn a_cancelled_command_keeps_its_replay_and_artifact() {
    let fixture = Fixture::new();
    let log =
        LegacyLog::started_007("legacy-cancelled").turn(&cancelled_turn_007("cancelled", RUN_CALL));
    let (events, _) = written(&fixture, &log);
    let Some(ConversationEvent::Interrupted(interrupted)) = events.last() else {
        panic!("{events:?}");
    };
    assert_eq!(
        (
            interrupted.command_replay_ref.as_deref(),
            interrupted.command_replay_bytes,
            interrupted.command_artifact_ref.as_deref()
        ),
        (
            Some("fx-command-replay-0c0d.bin"),
            Some(40),
            Some("result-run_command-0011-2233.txt")
        )
    );
}

#[test]
fn a_cancelled_command_upstream_would_refuse_hides_the_session() {
    let fixture = Fixture::new();
    let read_call = RUN_CALL.replace("run_command", "read_file");
    for turn in [
        cancelled_turn_007("failed", RUN_CALL),
        cancelled_turn_007("cancelled", "null"),
        cancelled_turn_007("cancelled", &read_call),
    ] {
        let log = LegacyLog::started_007("legacy-refused").turn(&turn);
        assert!(fixture.summary(&log).is_err(), "{turn}");
    }
}

fn base64_007(bytes: &[u8]) -> String {
    format!(
        "{{\"encoding\":\"base64\",\"data\":\"{}\"}}",
        STANDARD.encode(bytes)
    )
}

#[test]
fn output_that_is_not_utf8_is_stored_byte_for_byte_behind_a_utf8_preview() {
    let fixture = Fixture::new();
    let mut output = vec![b'a'; 5_000];
    output.extend([0xff, 0xfe, b'\n']);
    let log = LegacyLog::started_007("legacy-binary").turn(&turn_007(
        "call_1",
        "run_command",
        "{\"command\":\"cat blob\"}",
        &result_007(
            "call_1",
            "run_command",
            &base64_007(&output),
            "null",
            "null",
        ),
    ));
    let (events, _) = written(&fixture, &log);
    let result = results(&events)[0];
    assert_eq!(result.preview.as_deref(), Some("a".repeat(4_096).as_str()));
    assert_eq!(result.stored_bytes, 5_003);
    assert_eq!(
        tool_results_file(&fixture, "legacy-binary", &result.artifact_ref),
        output
    );
    assert!(
        result
            .artifact_ref
            .ends_with(&format!("-{}.txt", digest_hex(&output)))
    );
}

#[test]
fn text_upstream_cannot_write_as_utf8_hides_the_session() {
    let fixture = Fixture::new();
    let short_output = turn_007(
        "call_1",
        "run_command",
        "{\"command\":\"cat blob\"}",
        &result_007(
            "call_1",
            "run_command",
            &base64_007(b"ok \xff"),
            "null",
            "null",
        ),
    );
    let binary_prompt = reply_007("x", "done").replace(
        "\"text\":\"x\"",
        &format!("\"text\":{}", base64_007(b"\xff")),
    );
    let binary_content = edit_turn_007(&presentation_007("x", "y").replace(
        "\"after_content\":\"y\"",
        &format!("\"after_content\":{}", base64_007(b"\xff")),
    ));
    let terminal_action = turn_007(
        "call_1",
        "terminal",
        "{\"action\":\"start\",\"command\":\"top\"}",
        &result_007("call_1", "terminal", "\"started\"", "null", "null").replace(
            "\"terminal_action_presentation\":null",
            "\"terminal_action_presentation\":{\"kind\":\"returned\",\"outcome\":{\"kind\":\"started\",\"value\":null}}",
        ),
    );
    for turn in [short_output, binary_prompt, binary_content, terminal_action] {
        let log = LegacyLog::started_007("legacy-hidden").turn(&turn);
        assert!(fixture.summary(&log).is_err(), "{turn}");
    }
}

#[test]
fn provider_replays_are_kept_on_the_steps_and_reply_that_saved_them() {
    let fixture = Fixture::new();
    let replay = |parts: &str| {
        format!(
            "{{\"source\":{{\"provider\":\"gateway\",\"model\":\"openai/gpt-5\"}},\"parts_json\":{}}}",
            serde_json::to_string(parts).unwrap()
        )
    };
    let turn = format!(
        "{{\"kind\":\"assistant\",\"user\":{{\"text\":\"think\",\"images\":[]}},\"assistant\":\"\",\"execution\":{{\"schema_version\":9,\"tool_steps\":[{{\"assistant\":null,\"tool_calls\":[],\"tool_results\":[],\"provider_replay\":{}}}],\"files\":[],\"steering\":[],\"turn_summary\":null}},\"provider_replay\":{}}}",
        replay("[{\"type\":\"step\"}]"),
        replay("[{\"type\":\"reply\"}]")
    );
    let log = LegacyLog::started_007("legacy-replays").turn(&turn);
    let (events, _) = written(&fixture, &log);
    let replays: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ConversationEvent::Assistant(assistant) => Some((
                assistant
                    .provider_replay
                    .as_ref()
                    .map(|replay| replay.parts_json.as_str()),
                assistant.standalone_response,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        replays,
        [
            (Some("[{\"type\":\"step\"}]"), true),
            (Some("[{\"type\":\"reply\"}]"), false)
        ]
    );
}

#[test]
fn a_large_edit_whose_contents_are_not_utf8_moves_them_as_bytes() {
    let fixture = Fixture::new();
    let mut after = vec![b'a'; 5_000];
    after.push(0xff);
    let presentation = presentation_007("x", "y").replace(
        "\"after_content\":\"y\"",
        &format!("\"after_content\":{}", base64_007(&after)),
    );
    let log = LegacyLog::started_007("legacy-binary-edit").turn(&edit_turn_007(&presentation));
    let (events, _) = written(&fixture, &log);
    let bytes = after
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let pack = format!("{{\"previous_content\":\"x\",\"after_content\":[{bytes}]}}");
    let handle = format!(
        "diff-{}-{}.json",
        digest_hex(EDIT_CALL.as_bytes()),
        digest_hex(pack.as_bytes())
    );
    assert_eq!(
        results(&events)[0].committed_file_presentation.as_deref(),
        Some(&expected_presentation(None, None, Some(handle.clone())))
    );
    assert_eq!(
        tool_results_file(&fixture, "legacy-binary-edit", &handle),
        pack.into_bytes()
    );
}

#[test]
fn a_provider_replay_upstream_would_refuse_hides_the_session() {
    let fixture = Fixture::new();
    let turn = |model: &str, parts: &str| {
        format!(
            "{{\"kind\":\"assistant\",\"user\":{{\"text\":\"think\",\"images\":[]}},\"assistant\":\"done\",\"execution\":{{\"schema_version\":9,\"tool_steps\":[{{\"assistant\":\"step\",\"tool_calls\":[],\"tool_results\":[],\"provider_replay\":{{\"source\":{{\"provider\":\"gateway\",\"model\":\"{model}\"}},\"parts_json\":\"{parts}\"}}}}],\"files\":[],\"steering\":[],\"turn_summary\":null}}}}"
        )
    };
    let long_model = "m".repeat(257);
    for case in [
        turn("", "[]"),
        turn("openai/gpt-5", ""),
        turn(&long_model, "[]"),
    ] {
        let log = LegacyLog::started_007("legacy-bad-replay").turn(&case);
        assert!(fixture.summary(&log).is_err(), "{case}");
    }
}
