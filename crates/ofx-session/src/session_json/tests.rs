use ofx_contract::{
    CommandProcessPresentation, FileEvidenceAction, ToolArgumentIntegrity, ToolResultStatus,
};

use super::*;

fn fetched() -> ToolResultEvent {
    let mut result = ToolResultEvent::new(
        "fetch_1",
        "web_fetch",
        ToolResultStatus::Success,
        "result-web.txt",
        48,
        ArtifactCompleteness::Complete,
    );
    result.preview = Some("<artifact_handle>artifact-file.pdf</artifact_handle>".to_owned());
    result.output_bytes = Some(48);
    result.created_at_ms = 5;
    result.permission_feedback = vec!["approved".to_owned()];
    result.command_replay_ref = Some("fx-command-replay-private-sentinel.bin".to_owned());
    result.command_replay_bytes = Some(77);
    result.command_process_presentation = Some(CommandProcessPresentation::ExitCode(9));
    result
}

fn edited() -> ToolResultEvent {
    let mut result = ToolResultEvent::new(
        "edit_1",
        "edit_file",
        ToolResultStatus::Failure,
        "result-edit.txt",
        4,
        ArtifactCompleteness::Partial,
    );
    result.output_bytes = Some(10);
    result.committed_file_presentation = Some(Box::new(
        serde_json::from_str::<CommittedFilePresentation>(
            r#"{"path":"src/a.rs","kind":"edited","lines":[{"kind":"addition","new_line":2,"text":"new"}],"additions":1,"deletions":0,"truncated":false,"lifecycle_id":{"turn_id":4,"call_id":"edit_1"},"content_handle":"diff-abc.json"}"#,
        )
        .unwrap(),
    ));
    result
}

#[test]
fn execution_presentation_keeps_upstreams_shape_and_leaves_replays_and_summaries_out() {
    let execution = TurnExecution {
        steps: vec![
            ExecutedStep {
                assistant: Some("Fetching artifact.".to_owned()),
                calls: vec![ToolCallEvent::new(
                    "fetch_1",
                    "web_fetch",
                    "{\"url\":\"https://example.com/file.pdf\"}",
                    ToolArgumentIntegrity::Valid,
                )],
                results: vec![fetched()],
            },
            ExecutedStep {
                assistant: None,
                calls: vec![ToolCallEvent::new(
                    "edit_1",
                    "edit_file",
                    "{}",
                    ToolArgumentIntegrity::Valid,
                )],
                results: vec![edited()],
            },
        ],
        files: vec![FileEvidence {
            path: "src/a.rs".to_owned(),
            new_path: None,
            tool_call_id: "edit_1".to_owned(),
            tool_name: "edit_file".to_owned(),
            action: FileEvidenceAction::Edit,
            status: ToolResultStatus::Failure,
            model_view_covers_full_file: false,
            stale: true,
        }],
        steering: vec![ArchivedSteering {
            text: "stop".to_owned(),
            assistant_prefix: None,
            after_tool_step_count: 2,
        }],
    };
    assert_eq!(
        execution.presentation_json().to_string(),
        concat!(
            "{\"schema_version\":3,\"tool_steps\":[",
            "{\"assistant\":\"Fetching artifact.\",\"tool_calls\":[{\"id\":\"fetch_1\",\"name\":\"web_fetch\",\"arguments_json\":\"{\\\"url\\\":\\\"https://example.com/file.pdf\\\"}\",\"provider_result\":null}],",
            "\"tool_results\":[{\"tool_call_id\":\"fetch_1\",\"tool_name\":\"web_fetch\",\"status\":\"success\",\"output\":\"<artifact_handle>artifact-file.pdf</artifact_handle>\",\"output_handle\":\"result-web.txt\",\"preview\":\"<artifact_handle>artifact-file.pdf</artifact_handle>\",\"output_bytes\":48,\"stored_output_bytes\":48,\"truncated\":false,\"provider_native\":false,\"created_at_ms\":5,\"permission_feedback\":[\"approved\"]}]},",
            "{\"assistant\":null,\"tool_calls\":[{\"id\":\"edit_1\",\"name\":\"edit_file\",\"arguments_json\":\"{}\",\"provider_result\":null}],",
            "\"tool_results\":[{\"tool_call_id\":\"edit_1\",\"tool_name\":\"edit_file\",\"status\":\"failure\",\"output\":\"\",\"output_handle\":\"result-edit.txt\",\"output_bytes\":10,\"stored_output_bytes\":4,\"truncated\":true,\"provider_native\":false,\"created_at_ms\":0,\"permission_feedback\":[],",
            "\"committed_file_presentation\":{\"path\":\"src/a.rs\",\"kind\":\"edited\",\"lines\":[{\"kind\":\"addition\",\"old_line\":null,\"new_line\":2,\"text\":\"new\"}],\"additions\":1,\"deletions\":0,\"truncated\":false,\"previous_content\":null,\"after_content\":null,\"lifecycle_id\":{\"turn_id\":4,\"call_id\":\"edit_1\"},\"content_handle\":\"diff-abc.json\"}}]}],",
            "\"files\":[{\"path\":\"src/a.rs\",\"new_path\":null,\"tool_call_id\":\"edit_1\",\"tool_name\":\"edit_file\",\"action\":\"edit\",\"status\":\"failure\",\"model_view_covers_full_file\":false,\"stale\":true}],",
            "\"steering\":[{\"text\":\"stop\",\"assistant_prefix\":null,\"after_tool_step_count\":2}]}"
        )
    );
}

#[test]
fn a_result_without_a_preview_shows_empty_output() {
    assert_eq!(edited().output(), "");
    assert_eq!(
        fetched().output(),
        "<artifact_handle>artifact-file.pdf</artifact_handle>"
    );
}
