use std::fs;

use super::*;
use crate::test_sources::{CapturedImages, user_with_images};

fn raw_input(messages: &[ChatMessage]) -> Result<String> {
    let mut out = String::new();
    write_input(
        &mut out,
        messages,
        &vec![None; messages.len()],
        REPLAY_LIMITS,
    )?;
    Ok(out)
}

#[test]
fn responses_write_a_user_image_after_its_text_as_upstream_does() {
    let images = CapturedImages::new();
    let image = images.capture(1, "first.png", b"\x89PNG\r\n\x1a\nA");

    assert_eq!(
        raw_input(&[user_with_images("first", vec![image])]).unwrap(),
        concat!(
            r#"{"role":"user","content":[{"type":"input_text","text":"first"},"#,
            r#"{"type":"input_image","detail":"auto","image_url":"data:image/png;base64,iVBORw0KGgpB"}]}"#,
        )
    );
}

#[test]
fn responses_images_remain_on_their_owning_users_across_tools_and_later_prompts() {
    let images = CapturedImages::new();
    let first = images.capture(1, "first.png", b"\x89PNG\r\n\x1a\nA");
    let second = images.capture(2, "second.png", b"\x89PNG\r\n\x1a\nB");
    let messages = [
        user_with_images("first", vec![first.clone()]),
        assistant(None, vec![call("read_1", "read_file", "{}")]),
        tool_result("read_1", "read result"),
        user_with_images("second", vec![second, first]),
        assistant(Some("response"), Vec::new()),
        ChatMessage::user("continue"),
    ];

    let items = input(&messages, &[None; 6]).unwrap();

    assert_eq!(items.len(), 6);
    let image = |data: &str| json!({"type": "input_image", "detail": "auto", "image_url": format!("data:image/png;base64,{data}")});
    assert_eq!(
        items[0]["content"],
        json!([{"type": "input_text", "text": "first"}, image("iVBORw0KGgpB")])
    );
    assert_eq!(
        items[3]["content"],
        json!([
            {"type": "input_text", "text": "second"},
            image("iVBORw0KGgpC"),
            image("iVBORw0KGgpB"),
        ])
    );
    assert_eq!(items[2]["output"], "read result");
    assert_eq!(
        items[5]["content"],
        json!([{"type": "input_text", "text": "continue"}])
    );
}

#[test]
fn responses_images_without_text_are_the_only_parts() {
    let images = CapturedImages::new();
    let image = images.capture(1, "only.png", b"\x89PNG\r\n\x1a\nA");

    let items = input(&[user_with_images("", vec![image.clone(), image])], &[None]).unwrap();

    assert_eq!(
        items[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["input_image", "input_image"]
    );
}

#[test]
fn responses_images_use_captured_bytes_and_reject_unavailable_snapshots() {
    let images = CapturedImages::new();
    let attachment = images.capture(1, "source.png", b"\x89PNG\r\n\x1a\nA");
    let messages = [user_with_images("", vec![attachment.clone()])];
    fs::remove_file(&attachment.path).unwrap();

    assert!(
        raw_input(&messages)
            .unwrap()
            .contains("data:image/png;base64,iVBORw0KGgpB")
    );
    let snapshot = attachment.snapshot_path.as_deref().unwrap();
    fs::write(snapshot, b"\x89PNG\r\n\x1a\nB").unwrap();
    assert_eq!(
        raw_input(&messages),
        Err(ResponsesError::Image(AttachmentError::ImageSnapshotCorrupt))
    );
    fs::remove_file(snapshot).unwrap();
    assert_eq!(
        raw_input(&messages),
        Err(ResponsesError::Image(AttachmentError::FileNotFound))
    );
    assert_eq!(
        ResponsesError::Image(AttachmentError::FileNotFound).to_string(),
        "FileNotFound"
    );
}
