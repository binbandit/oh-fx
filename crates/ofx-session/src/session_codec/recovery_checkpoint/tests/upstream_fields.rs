use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ofx_config::{PrivateDir, ProviderId};
use ofx_contract::{
    CommandOutputReplay, CommittedFilePresentation, FilePresentationKind, FilePresentationLine,
    FilePresentationLineKind, ImageAttachment, ModelRecoveryAction, ModelRecoveryCause,
    PersistedResult, RecoveryPoint, RecoveryProgress, RecoveryToolState, ToolExecutionProvenance,
    ToolImage, ToolImages, ToolLifecycleId, ToolResultStatus, TurnId,
};

use super::{decoded, file_with};
use crate::result_store::RESULT_UNAVAILABLE;
use crate::session_codec::SavedProvider;
use crate::session_codec::recovery_checkpoint::upstream_fixture::{
    CONFIGURED_IDENTITY, EDIT_HANDLE, PNG_DATA, PNG_SHA256, REPLAY_HANDLE, fields_checkpoint,
    image_user, presentation,
};
use crate::session_codec::recovery_checkpoint::{
    CheckpointSource, RecoveryCheckpoint, RouteCredential, SavedOutput, decode_recovery_file,
    encode_recovery_file,
};
use crate::session_error::SessionError;

fn rejected(text: &str) {
    assert_eq!(
        decode_recovery_file(&file_with(text, 1), 1),
        Err(SessionError::InvalidRecoveryCheckpoint),
        "{text:.600}"
    );
}

fn edit_presentation() -> CommittedFilePresentation {
    CommittedFilePresentation {
        path: "a.rs".to_owned(),
        kind: FilePresentationKind::Edited,
        lines: vec![
            FilePresentationLine {
                kind: FilePresentationLineKind::Deletion,
                old_line: Some(1),
                new_line: None,
                text: "old".to_owned(),
            },
            FilePresentationLine {
                kind: FilePresentationLineKind::Addition,
                old_line: None,
                new_line: Some(1),
                text: "new".to_owned(),
            },
        ],
        additions: 1,
        deletions: 1,
        truncated: false,
        previous_content: Some("old\n".to_owned()),
        after_content: Some("new\n".to_owned()),
        lifecycle_id: Some(ToolLifecycleId {
            turn_id: 7,
            call_id: "call_edit".to_owned(),
        }),
        content_handle: None,
    }
}

fn png() -> Vec<u8> {
    STANDARD.decode(PNG_DATA).unwrap()
}

fn with_image(image: &str) -> String {
    fields_checkpoint().replace(
        &image_user(),
        &format!("{{\"text\":\"fix the build\",\"images\":[{image}]}}"),
    )
}

fn with_presentation(presentation_json: &str) -> String {
    fields_checkpoint().replace(presentation(), presentation_json)
}

fn tool_result_tail() -> &'static str {
    "\"terminal_action_presentation\":null,\"tool_images\":[{\"type\":\"image\",\"mimeType\":\"image/png\",\"data\":\"iVBORw0KGgoAAAANSUhEUg==\"}]}"
}

fn with_tool_images(tail: &str) -> String {
    let checkpoint = fields_checkpoint();
    assert!(checkpoint.contains(tool_result_tail()));
    checkpoint.replace(tool_result_tail(), tail)
}

fn shot(checkpoint: &RecoveryCheckpoint) -> (&str, &ToolImages) {
    let result = &checkpoint.execution.tool_steps[0].tool_results[3];
    (&result.output, &result.persisted.tool_images)
}

#[test]
fn every_field_fx_saves_in_a_checkpoint_reads_back() {
    let checkpoint = decoded(&fields_checkpoint());
    assert_eq!(
        checkpoint.images,
        [ImageAttachment {
            id: 1,
            path: "/Users/me/shot.png".to_owned(),
            media_type: "image/png".to_owned(),
            snapshot_path: Some("images/shot-1.png".to_owned()),
            snapshot_sha256: Some(PNG_SHA256.to_owned()),
            inline_data: None,
            source_ref: None,
        }]
    );
    let step = &checkpoint.execution.tool_steps[0];
    let search = &step.tool_calls[2];
    assert_eq!(search.provider_result.as_deref(), Some("{\"results\":[]}"));
    assert_eq!(search.provenance, ToolExecutionProvenance::FxLocal);
    let [edit, ls, native, shot] = &step.tool_results[..] else {
        panic!("four results");
    };
    assert_eq!(
        edit.persisted,
        PersistedResult {
            output_handle: Some(EDIT_HANDLE.to_owned()),
            preview: Some("Edited a.rs".to_owned()),
            stored_output_bytes: 5000,
            truncated: true,
            created_at_ms: 1_700_000_000_001,
            provider_native: false,
            committed_file_presentation: Some(edit_presentation()),
            command_output_replay: None,
            tool_images: ToolImages::None,
        }
    );
    assert_eq!(
        ls.persisted.command_output_replay,
        Some(CommandOutputReplay::Available {
            handle: REPLAY_HANDLE.to_owned(),
            framed_bytes: 22,
        })
    );
    assert_eq!(ls.persisted.created_at_ms, 1_700_000_000_002);
    assert!(native.persisted.provider_native);
    assert_eq!(
        shot.persisted.tool_images,
        ToolImages::Inline(vec![ToolImage {
            data: PNG_DATA.to_owned(),
            mime_type: "image/png".to_owned(),
            source_ref: None,
        }])
    );
    assert_eq!(shot.output, "Captured.");
}

#[test]
fn checkpoint_images_follow_upstreams_snapshot_and_inline_rules() {
    let inline = decoded(&with_image(&format!(
        "{{\"id\":2,\"path\":\"clip.png\",\"media_type\":\"image/png\",\"snapshot_path\":null,\"snapshot_sha256\":\"{PNG_SHA256}\",\"inline_data\":{{\"encoding\":\"base64\",\"data\":\"{PNG_DATA}\"}}}}"
    )));
    assert_eq!(inline.images[0].inline_data, Some(png()));
    let sparse = decoded(&with_image(
        "{\"id\":3,\"path\":\"a.png\",\"media_type\":\"image/png\"}",
    ));
    assert_eq!(sparse.images[0].snapshot_path, None);
    let referenced = decoded(&with_image(
        "{\"id\":4,\"path\":\"a.png\",\"media_type\":\"image/png\",\"snapshot_path\":\"images/a.png\",\"snapshot_sha256\":\"x\",\"source_ref\":\"host:clip\"}",
    ));
    assert_eq!(
        referenced.images[0].source_ref.as_deref(),
        Some("host:clip")
    );
    let inline_png = format!("{{\"encoding\":\"base64\",\"data\":\"{PNG_DATA}\"}}");
    for image in [
        format!(
            "{{\"id\":2,\"path\":\"clip.png\",\"media_type\":\"image/png\",\"snapshot_sha256\":\"x\",\"inline_blob\":0}}"
        ),
        format!(
            "{{\"id\":2,\"path\":\"clip.png\",\"media_type\":\"image/png\",\"snapshot_sha256\":\"x\",\"inline_data\":{inline_png},\"inline_blob\":0}}"
        ),
        format!(
            "{{\"id\":2,\"path\":\"clip.png\",\"media_type\":\"image/png\",\"snapshot_path\":\"images/a.png\",\"snapshot_sha256\":\"x\",\"inline_data\":{inline_png}}}"
        ),
        format!(
            "{{\"id\":2,\"path\":\"clip.png\",\"media_type\":\"image/png\",\"inline_data\":{inline_png}}}"
        ),
        format!(
            "{{\"id\":2,\"path\":\"clip.gif\",\"media_type\":\"image/gif\",\"snapshot_sha256\":\"x\",\"inline_data\":{inline_png}}}"
        ),
        "{\"id\":2,\"path\":\"clip.png\",\"media_type\":\"image/png\",\"snapshot_sha256\":\"x\",\"inline_data\":\"\"}".to_owned(),
        "{\"id\":2,\"path\":\"a.png\",\"media_type\":\"image/png\",\"snapshot_path\":\"images/a.png\"}".to_owned(),
        "{\"id\":2,\"path\":\"a.png\",\"media_type\":\"image/png\",\"snapshot_sha256\":\"x\"}".to_owned(),
        "{\"id\":2,\"path\":\"a.png\",\"media_type\":\"image/png\",\"source_ref\":null}".to_owned(),
        "{\"id\":2,\"path\":\"a.png\",\"media_type\":\"image/png\",\"source_ref\":\"bad\\nref\"}".to_owned(),
        "{\"id\":2,\"path\":\"a.png\",\"media_type\":\"image/png\",\"source_ref\":1}".to_owned(),
        "{\"path\":\"a.png\",\"media_type\":\"image/png\"}".to_owned(),
        "{\"id\":2,\"media_type\":\"image/png\"}".to_owned(),
        "{\"id\":2,\"path\":\"a.png\"}".to_owned(),
        "{\"id\":-1,\"path\":\"a.png\",\"media_type\":\"image/png\"}".to_owned(),
        "{\"id\":2,\"path\":\"a.png\",\"media_type\":\"image/png\",\"extra\":1}".to_owned(),
    ] {
        rejected(&with_image(&image));
    }
    rejected(
        &fields_checkpoint().replace(&image_user(), "{\"text\":\"fix the build\",\"images\":{}}"),
    );
}

#[test]
fn file_presentations_read_in_both_shapes_upstream_writes() {
    let handled = presentation().replace(
        "\"previous_content\":\"old\\n\",\"after_content\":\"new\\n\",\"lifecycle_id\":{\"turn_id\":7,\"call_id\":\"call_edit\"}}",
        "\"previous_content\":null,\"after_content\":null,\"lifecycle_id\":null,\"content_handle\":\"diff-call_edit-0011.json\"}",
    );
    let read = decoded(&with_presentation(&handled));
    let kept = read.execution.tool_steps[0].tool_results[0]
        .persisted
        .committed_file_presentation
        .clone()
        .unwrap();
    assert_eq!(
        kept.content_handle.as_deref(),
        Some("diff-call_edit-0011.json")
    );
    assert_eq!(kept.lifecycle_id, None);
    let null_handle = presentation().replace("}}", "},\"content_handle\":null}");
    let read = decoded(&with_presentation(&null_handle));
    assert_eq!(
        read.execution.tool_steps[0].tool_results[0]
            .persisted
            .committed_file_presentation,
        Some(edit_presentation())
    );
    let durable = presentation().replace(
        "\"text\":\"new\"",
        "\"text\":{\"encoding\":\"base64\",\"data\":\"bmV3\"}",
    );
    assert_eq!(
        decoded(&with_presentation(&durable)).execution.tool_steps[0].tool_results[0]
            .persisted
            .committed_file_presentation,
        Some(edit_presentation())
    );
    for invalid in [
        presentation().replace("}}", "},\"content_handle\":\"diff-a.json\"}"),
        handled.replace("diff-call_edit-0011.json", "result-a.txt"),
        presentation().replace("\"kind\":\"edited\"", "\"kind\":\"renamed\""),
        presentation().replace("\"kind\":\"addition\"", "\"kind\":\"moved\""),
        presentation().replace(
            ",\"lifecycle_id\":{\"turn_id\":7,\"call_id\":\"call_edit\"}",
            "",
        ),
        presentation().replace("\"old_line\":1", "\"old_line\":-1"),
        presentation().replace("\"old_line\":1", "\"old_line\":4294967296"),
        presentation().replace("\"additions\":1", "\"additions\":1,\"extra\":1"),
        presentation().replace(
            "\"call_id\":\"call_edit\"}",
            "\"call_id\":\"call_edit\",\"x\":1}",
        ),
        presentation().replace("\"text\":\"old\"", "\"text\":null"),
    ] {
        rejected(&with_presentation(&invalid));
    }
}

#[test]
fn command_output_replays_read_as_upstream_writes_them() {
    let replay =
        format!("{{\"kind\":\"available\",\"handle\":\"{REPLAY_HANDLE}\",\"framed_bytes\":22}}");
    let unavailable = decoded(&fields_checkpoint().replace(&replay, "{\"kind\":\"unavailable\"}"));
    assert_eq!(
        unavailable.execution.tool_steps[0].tool_results[1]
            .persisted
            .command_output_replay,
        Some(CommandOutputReplay::Unavailable)
    );
    for invalid in [
        "{\"kind\":\"unavailable\",\"handle\":\"x\"}",
        "{\"kind\":\"available\",\"handle\":\"x\"}",
        "{\"kind\":\"available\",\"handle\":null,\"framed_bytes\":1}",
        "{\"kind\":\"lost\"}",
        "{\"handle\":\"x\",\"framed_bytes\":1}",
        "\"available\"",
    ] {
        rejected(&fields_checkpoint().replace(&replay, invalid));
    }
}

#[test]
fn tool_images_and_their_handles_read_as_upstream_writes_them() {
    let stored = decoded(&with_tool_images(
        "\"terminal_action_presentation\":null,\"tool_image_handle\":\"image-result-screenshot-1-2.txt\"}",
    ));
    assert_eq!(
        shot(&stored),
        (
            "Captured.",
            &ToolImages::Stored("image-result-screenshot-1-2.txt".to_owned())
        )
    );
    let cleared = decoded(&with_tool_images(
        "\"terminal_action_presentation\":null,\"tool_image_handle\":null}",
    ));
    assert_eq!(shot(&cleared), ("Captured.", &ToolImages::None));
    let broken = decoded(&with_tool_images(
        "\"terminal_action_presentation\":null,\"tool_images\":[{\"type\":\"image\",\"mimeType\":\"image/gif\",\"data\":\"iVBORw0KGgoAAAANSUhEUg==\"}]}",
    ));
    assert_eq!(
        shot(&broken),
        (
            "Captured.\n[Saved tool image unavailable: InvalidImage]",
            &ToolImages::None
        )
    );
    let unsupported = decoded(&with_tool_images(
        "\"terminal_action_presentation\":null,\"tool_images\":[{\"type\":\"text\",\"text\":\"x\"}]}",
    ));
    assert_eq!(
        shot(&unsupported),
        (
            "Captured.\n[Saved tool image unavailable: unsupported content]",
            &ToolImages::None
        )
    );
    for invalid in [
        "\"terminal_action_presentation\":null,\"tool_image_handle\":\"i\",\"tool_images\":[]}",
        "\"terminal_action_presentation\":null,\"tool_image_handle\":null,\"tool_images\":[]}",
        "\"terminal_action_presentation\":null,\"tool_images\":{}}",
        "\"terminal_action_presentation\":null,\"tool_images\":[{\"type\":\"image\",\"mimeType\":\"image/png\",\"data\":\"iVBORw0KGgoAAAANSUhEUg==\",\"sourceRef\":\"host:a\"}]}",
        "\"terminal_action_presentation\":null,\"tool_image_handle\":1}",
    ] {
        rejected(&with_tool_images(invalid));
    }
}

#[test]
fn checkpoint_tool_arguments_are_repaired_as_upstream_repairs_them() {
    let search = "{\"id\":\"call_search\",\"name\":\"web_search\",\"arguments_json\":\"{\\\"query\\\":\\\"zig\\\"}\",\"provider_result\":\"{\\\"results\\\":[]}\"}";
    let provided = decoded(&fields_checkpoint().replace(
        search,
        "{\"id\":\"call_search\",\"name\":\"web_search\",\"arguments_json\":\"\\\"zig\\\"\",\"provider_result\":\"{\\\"results\\\":[]}\"}",
    ));
    let call = &provided.execution.tool_steps[0].tool_calls[2];
    assert_eq!(call.provenance, ToolExecutionProvenance::ProviderExecuted);
    assert_eq!(call.arguments, "\"zig\"");
    let native = decoded(&fields_checkpoint().replace(
        search,
        "{\"id\":\"call_search\",\"name\":\"web_search\",\"arguments_json\":\"[1]\",\"provider_result\":null}",
    ));
    let call = &native.execution.tool_steps[0].tool_calls[2];
    assert_eq!(call.provenance, ToolExecutionProvenance::ProviderExecuted);
    assert_eq!(call.arguments, "[1]");
    let ls = "\"arguments_json\":\"{\\\"command\\\":\\\"ls\\\"}\"";
    let local = decoded(&fields_checkpoint().replace(ls, "\"arguments_json\":\"[1]\""));
    let call = &local.execution.tool_steps[0].tool_calls[1];
    assert_eq!(call.provenance, ToolExecutionProvenance::FxLocal);
    assert_eq!(call.arguments, "{}");
    let malformed =
        decoded(&fields_checkpoint().replace(ls, "\"arguments_json\":\"{\\\"command\\\":\""));
    let step = &malformed.execution.tool_steps[0];
    assert_eq!(step.tool_calls[1].arguments, "{}");
    let refused = &step.tool_results[1];
    let expected = malformed_tool_arguments_json_without_diagnostic("shell");
    assert_eq!(refused.status, ToolResultStatus::Failure);
    assert_eq!(refused.output, expected);
    assert_eq!(refused.output_bytes, expected.len());
    assert_eq!(refused.persisted.stored_output_bytes, expected.len() as u64);
    assert_eq!(refused.persisted.output_handle, None);
    assert_eq!(
        refused.persisted.command_output_replay,
        Some(CommandOutputReplay::Available {
            handle: REPLAY_HANDLE.to_owned(),
            framed_bytes: 22,
        })
    );
    let unanswered = fields_checkpoint().replace(ls, "\"arguments_json\":\"[1]\"");
    let start = unanswered.find("{\"tool_call_id\":\"call_ls\"").unwrap();
    let end = unanswered[start..]
        .find(",{\"tool_call_id\":\"call_search\"")
        .unwrap()
        + start;
    rejected(&format!(
        "{}{}",
        &unanswered[..start - 1],
        &unanswered[end..]
    ));
}

fn malformed_tool_arguments_json_without_diagnostic(tool_name: &str) -> String {
    ofx_contract::tool_execution_failure_json(&ofx_contract::ExecutionFailure {
        tool_name,
        message: "Tool arguments were not valid JSON.",
        details: &[],
        suggestion: Some(
            "Reissue the tool call with complete valid JSON arguments matching the tool schema.",
        ),
    })
}

#[test]
fn review_feedback_is_refused_on_a_provider_native_result() {
    rejected(
        &fields_checkpoint().replace(
            "\"status\":\"success\",\"output\":\"{\\\"results\\\":[]}\",\"output_handle\":null,\"preview\":null,\"output_bytes\":14,\"stored_output_bytes\":14,\"truncated\":false,\"provider_native\":true,\"review_feedback\":false",
            "\"status\":\"failure\",\"output\":\"{\\\"results\\\":[]}\",\"output_handle\":null,\"preview\":null,\"output_bytes\":14,\"stored_output_bytes\":14,\"truncated\":false,\"provider_native\":true,\"review_feedback\":true",
        ),
    );
}

#[test]
fn a_result_whose_stored_output_is_gone_restores_as_unavailable_and_truncated() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open_or_create(&root.path().join("session")).unwrap();
    let text = fields_checkpoint().replace("\"truncated\":true", "\"truncated\":false");
    let mut checkpoint = decoded(&text);
    checkpoint.restore_outputs(&dir);
    let edit = &checkpoint.execution.tool_steps[0].tool_results[0];
    assert_eq!(edit.output, RESULT_UNAVAILABLE);
    assert!(edit.persisted.truncated);
    let mut truncated = decoded(&fields_checkpoint());
    truncated.restore_outputs(&dir);
    let edit = &truncated.execution.tool_steps[0].tool_results[0];
    assert!(
        edit.output.starts_with(&format!(
            "<tool_result_preview handle=\"{EDIT_HANDLE}\" stored_bytes=\"5000\">\nEdited a.rs\n"
        )),
        "{}",
        edit.output
    );
}

#[test]
fn a_continuation_carries_the_turns_images_and_each_results_persisted_fields() {
    let checkpoint = decoded(&fields_checkpoint());
    let images = checkpoint.images.clone();
    let persisted: Vec<_> = checkpoint.execution.tool_steps[0]
        .tool_results
        .iter()
        .map(|result| result.persisted.clone())
        .collect();
    let gateway = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    let continued = checkpoint.into_continuation(&gateway, "openai/gpt-5", false);
    assert_eq!(continued.images, images);
    let carried: Vec<_> = continued
        .outputs
        .iter()
        .map(|output| *output.persisted.clone().unwrap())
        .collect();
    assert_eq!(carried, persisted);
    let ofx_contract::ChatMessage::Assistant { tool_calls, .. } = &continued.messages[0] else {
        panic!("the restored step");
    };
    assert_eq!(
        tool_calls[2].provider_result.as_deref(),
        Some("{\"results\":[]}")
    );
}

fn written_back(checkpoint: &RecoveryCheckpoint) -> String {
    let turn = checkpoint.interrupted_turn();
    let point = RecoveryPoint {
        turn_id: TurnId::new(7),
        turn: ofx_contract::HistoryTurn {
            end: ofx_contract::TurnEnd::Replied {
                text: "",
                provider_replay: None,
            },
            ..turn
        },
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
    let outputs = point.turn.steps[0]
        .tool_results
        .iter()
        .map(|result| {
            let persisted = result.persisted.unwrap();
            SavedOutput {
                handle: persisted.output_handle.clone(),
                preview: persisted.preview.clone(),
                stored_bytes: persisted.stored_output_bytes,
            }
        })
        .collect();
    let source = CheckpointSource {
        point: &point,
        provider: &SavedProvider::new(ProviderId::Gateway, None).unwrap(),
        credential: Some(RouteCredential::configured()),
        replays: vec![None],
        outputs: vec![outputs],
        files: Vec::new(),
        work_id: None,
        created_at_ms: 99,
    };
    String::from_utf8(encode_recovery_file(4, &source).unwrap().unwrap()).unwrap()
}

#[test]
fn every_field_fx_saves_in_a_checkpoint_is_written_back_byte_for_byte() {
    let upstream = fields_checkpoint();
    assert!(upstream.contains(CONFIGURED_IDENTITY));
    assert_eq!(
        written_back(&decoded(&upstream)),
        String::from_utf8(file_with(&upstream, 4)).unwrap()
    );
    for variant in [
        with_tool_images(
            "\"terminal_action_presentation\":null,\"tool_image_handle\":\"image-result-screenshot-1-2.txt\"}",
        ),
        with_presentation(&presentation().replace(
            "\"previous_content\":\"old\\n\",\"after_content\":\"new\\n\",\"lifecycle_id\":{\"turn_id\":7,\"call_id\":\"call_edit\"}}",
            "\"previous_content\":null,\"after_content\":null,\"lifecycle_id\":null,\"content_handle\":\"diff-call_edit-0011.json\"}",
        )),
        with_image(&format!(
            "{{\"id\":2,\"path\":\"clip.png\",\"media_type\":\"image/png\",\"snapshot_path\":null,\"snapshot_sha256\":\"{PNG_SHA256}\",\"inline_data\":{{\"encoding\":\"base64\",\"data\":\"{PNG_DATA}\"}},\"source_ref\":\"host:clip\"}}"
        )),
    ] {
        assert_eq!(
            written_back(&decoded(&variant)),
            String::from_utf8(file_with(&variant, 4)).unwrap()
        );
    }
}

#[test]
fn an_absolute_snapshot_path_is_written_as_its_images_locator() {
    let absolute = with_image(&format!(
        "{{\"id\":1,\"path\":\"/Users/me/shot.png\",\"media_type\":\"image/png\",\"snapshot_path\":\"/Users/me/.fx/sessions/s/images/shot-1.png\",\"snapshot_sha256\":\"{PNG_SHA256}\"}}"
    ));
    let checkpoint = decoded(&absolute);
    assert_eq!(
        checkpoint.images[0].snapshot_path.as_deref(),
        Some("/Users/me/.fx/sessions/s/images/shot-1.png")
    );
    assert_eq!(
        written_back(&checkpoint),
        String::from_utf8(file_with(&fields_checkpoint(), 4)).unwrap()
    );
}
