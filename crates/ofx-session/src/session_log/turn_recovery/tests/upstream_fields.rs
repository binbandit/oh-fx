use ofx_contract::{
    ChatMessage, HistoryStep, HistoryTurn, ModelRecoveryAction, ModelRecoveryCause, RecoveredTurn,
    RecoveryPoint, RecoveryProgress, RecoveryToolState, StepResult, TurnEnd, TurnId,
};

use super::{Fixture, finished_turn, metadata};
use crate::result_store::make_handle;
use crate::session_codec::recovery_checkpoint::RouteCredential;
use crate::session_codec::recovery_checkpoint::upstream_fixture::{
    EDIT_HANDLE, PNG_SHA256, REPLAY_HANDLE, fields_checkpoint, presentation,
};
use crate::session_log::turn_recovery::RECOVERY_FILE;

const TOOL_IMAGES: &str =
    r#"[{"type":"image","mimeType":"image/png","data":"iVBORw0KGgoAAAANSUhEUg=="}]"#;

fn event(line: &str) -> &str {
    let start = line.find("\"event\":").unwrap() + "\"event\":".len();
    &line[start..line.len() - 1]
}

fn image_handle() -> String {
    format!(
        "image-{}",
        make_handle("call_shot", "screenshot", TOOL_IMAGES)
    )
}

fn upstream_turn_start() -> Vec<String> {
    let calls = [
        ("call_edit", "edit_file", r#"{\"path\":\"a.rs\"}"#, "null"),
        ("call_ls", "shell", r#"{\"command\":\"ls\"}"#, "null"),
        (
            "call_search",
            "web_search",
            r#"{\"query\":\"zig\"}"#,
            r#""{\"results\":[]}""#,
        ),
        ("call_shot", "screenshot", "{}", "null"),
    ];
    let mut events = vec![
        format!(
            r#"{{"user":{{"text":"fix the build","images":[{{"id":1,"path":"/Users/me/shot.png","media_type":"image/png","snapshot_path":"images/shot-1.png","snapshot_sha256":"{PNG_SHA256}","inline_data":null,"source_ref":null}}],"work_id":null}}}}"#
        ),
        r#"{"assistant":{"text":"Editing.","provider_replay":null,"standalone_response":false}}"#
            .to_owned(),
    ];
    events.extend(calls.map(|(id, name, arguments, provider_result)| {
        format!(
            r#"{{"tool_call":{{"call_id":"{id}","tool_name":"{name}","arguments_json":"{arguments}","argument_integrity":"valid","provisional_id":null,"provider_result":{provider_result},"final_identity":"valid","provenance":"fx_local"}}}}"#
        )
    }));
    let frame_presentation = presentation().replace("}}", "},\"content_handle\":null}");
    let ls = make_handle("call_ls", "shell", "a.rs\n");
    let search = make_handle("call_search", "web_search", "{\"results\":[]}");
    let shot = make_handle("call_shot", "screenshot", "Captured.");
    let image = image_handle();
    events.extend([
        format!(
            r#"{{"tool_result":{{"call_id":"call_edit","tool_name":"edit_file","status":"success","artifact_ref":"{EDIT_HANDLE}","tool_image_handle":null,"output_bytes":5000,"stored_bytes":5000,"completeness":"partial","preview":"Edited a.rs","provider_native":false,"created_at_ms":1700000000001,"permission_feedback":[],"committed_file_presentation":{frame_presentation},"command_replay_ref":null,"command_replay_bytes":null,"command_process_presentation":null,"terminal_action_presentation":null}}}}"#
        ),
        format!(
            r#"{{"tool_result":{{"call_id":"call_ls","tool_name":"shell","status":"success","artifact_ref":"{ls}","tool_image_handle":null,"output_bytes":5,"stored_bytes":5,"completeness":"complete","preview":"a.rs\n","provider_native":false,"created_at_ms":1700000000002,"permission_feedback":[],"committed_file_presentation":null,"command_replay_ref":"{REPLAY_HANDLE}","command_replay_bytes":22,"command_process_presentation":{{"exit_code":0}},"terminal_action_presentation":null}}}}"#
        ),
        format!(
            r#"{{"tool_result":{{"call_id":"call_search","tool_name":"web_search","status":"success","artifact_ref":"{search}","tool_image_handle":null,"output_bytes":14,"stored_bytes":14,"completeness":"complete","preview":"{{\"results\":[]}}","provider_native":true,"created_at_ms":1700000000003,"permission_feedback":[],"committed_file_presentation":null,"command_replay_ref":null,"command_replay_bytes":null,"command_process_presentation":null,"terminal_action_presentation":null}}}}"#
        ),
        format!(
            r#"{{"tool_result":{{"call_id":"call_shot","tool_name":"screenshot","status":"success","artifact_ref":"{shot}","tool_image_handle":"{image}","output_bytes":9,"stored_bytes":9,"completeness":"complete","preview":"Captured.","provider_native":false,"created_at_ms":1700000000004,"permission_feedback":[],"committed_file_presentation":null,"command_replay_ref":null,"command_replay_bytes":null,"command_process_presentation":null,"terminal_action_presentation":null}}}}"#
        ),
    ]);
    events
}

fn stored_tool_images(fixture: &Fixture) -> String {
    std::fs::read_to_string(fixture.path("tool-results").join(image_handle())).unwrap()
}

#[test]
fn a_committed_checkpoint_keeps_every_field_fx_saved_with_its_turn() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let prepared = fields_checkpoint().replace(
        "\"cause\":\"rate_limited\"",
        "\"cause\":\"compaction_prepared\"",
    );
    fixture.save_checkpoint(3, &prepared);
    let resumed = fixture.resume().unwrap();
    drop(resumed);
    let log = fixture.log();
    let mut expected = upstream_turn_start();
    expected.push(
        r#"{"interrupted":{"reason":"failed","partial_text":"Looking at","command_replay_ref":null,"command_replay_bytes":null,"command_artifact_ref":null,"files":[],"turn_summary":null}}"#
            .to_owned(),
    );
    let committed: Vec<&str> = log[3..].iter().map(|line| event(line)).collect();
    assert_eq!(committed, expected);
    assert_eq!(stored_tool_images(&fixture), TOOL_IMAGES);
    assert!(!fixture.path(RECOVERY_FILE).exists());
}

fn carried_history<'a>(continued: &'a RecoveredTurn, end: TurnEnd<'a>) -> HistoryTurn<'a> {
    let ChatMessage::Assistant {
        content,
        tool_calls,
        provider_replay,
    } = &continued.messages[0]
    else {
        panic!("the restored step");
    };
    let tool_results = continued.messages[1..]
        .iter()
        .zip(&continued.outputs)
        .map(|(message, output)| {
            let ChatMessage::Tool {
                call_id,
                tool_name,
                content,
                status,
            } = message
            else {
                panic!("a restored result");
            };
            StepResult {
                call_id: call_id.as_str(),
                tool_name,
                output: content,
                output_bytes: output.bytes,
                status: *status,
                model_view_covers_full_file: false,
                process: output.process,
                review_feedback: output.review_feedback,
                permission_feedback: Vec::new(),
                persisted: output.persisted.as_deref(),
            }
        })
        .collect();
    HistoryTurn {
        user: &continued.prompt,
        images: &continued.images,
        steps: vec![HistoryStep {
            assistant: content.as_deref().unwrap_or_default(),
            provider_replay: provider_replay.as_ref(),
            tool_calls,
            tool_results,
        }],
        steering: Vec::new(),
        files: &[],
        end,
    }
}

#[test]
fn a_continued_checkpoint_is_saved_again_byte_for_byte_and_then_with_its_turn() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    fixture.save_checkpoint(3, &fields_checkpoint());
    let provider = metadata().preferences.provider;
    let mut resumed = fixture.resume().unwrap();
    let continued = resumed
        .take_recovery()
        .unwrap()
        .into_turn(&provider, "openai/gpt-5", false);
    assert_eq!(
        continued.images[0].snapshot_path.as_deref(),
        Some("images/shot-1.png")
    );
    let point = RecoveryPoint {
        turn_id: TurnId::new(7),
        turn: carried_history(
            &continued,
            TurnEnd::Replied {
                text: "",
                provider_replay: None,
            },
        ),
        source: "Looking at",
        cause: ModelRecoveryCause::RateLimited,
        progress: RecoveryProgress::Waiting(ModelRecoveryAction::RetryingRequest),
        tool_state: RecoveryToolState::Confirmed,
        model: "openai/gpt-5",
        requested_fast_mode: false,
        fast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 1,
    };
    resumed
        .record_recovery(&point, &provider, RouteCredential::configured())
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.path(RECOVERY_FILE)).unwrap(),
        format!(
            "{{\"conversation_seq\":3,\"checkpoint\":{}}}\n",
            fields_checkpoint()
        )
    );
    let finished = carried_history(
        &continued,
        TurnEnd::Replied {
            text: "Done.",
            provider_replay: None,
        },
    );
    resumed.record_turn(&finished, &provider).unwrap();
    let log = fixture.log();
    let saved: Vec<&str> = log[3..].iter().map(|line| event(line)).collect();
    let mut expected = upstream_turn_start();
    expected.extend([
        r#"{"assistant":{"text":"Done.","provider_replay":null,"standalone_response":false}}"#
            .to_owned(),
        r#"{"turn_completed":{"files":[],"turn_summary":null}}"#.to_owned(),
    ]);
    assert_eq!(saved, expected);
    assert_eq!(stored_tool_images(&fixture), TOOL_IMAGES);
    assert!(!fixture.path(RECOVERY_FILE).exists());
}
