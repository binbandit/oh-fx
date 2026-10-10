use std::fs;

use super::*;
use crate::test_sources::{CapturedImages, user_with_images};

const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn image_request(messages: Vec<ChatMessage>) -> OwnedRequest {
    OwnedRequest {
        messages,
        ..test_request()
    }
}

#[test]
fn chat_completions_serializes_user_message_images_as_content_parts() {
    let images = CapturedImages::new();
    let pixel = images.capture(1, "pixel.png", &STANDARD.decode(PIXEL_PNG).unwrap());
    let request = image_request(vec![user_with_images("what is this", vec![pixel])]);

    let body = build(&request, ToolChoiceMode::Omit).unwrap();

    assert_eq!(
        body,
        format!(
            concat!(
                r#"{{"model":"opaque/local-model:8b","stream":true,"stream_options":{{"include_usage":true}},"#,
                r#""messages":[{{"role":"system","content":"first"}},{{"role":"system","content":"second"}},"#,
                r#"{{"role":"user","content":[{{"type":"text","text":"what is this"}},"#,
                r#"{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{}"}}}}]}}]}}"#,
            ),
            PIXEL_PNG
        )
    );
}

#[test]
fn chat_completions_keeps_images_on_their_owning_users_in_order() {
    let images = CapturedImages::new();
    let first = images.capture(1, "first.png", b"\x89PNG\r\n\x1a\nA");
    let second = images.capture(2, "second.png", b"\x89PNG\r\n\x1a\nB");
    let request = image_request(vec![
        user_with_images("", vec![first.clone()]),
        ChatMessage::Assistant {
            content: Some("seen".to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        },
        user_with_images("again", vec![second, first]),
        ChatMessage::user("continue"),
    ]);

    let body = build(&request, ToolChoiceMode::Omit).unwrap();

    let messages: Value = serde_json::from_str::<Value>(&body).unwrap()["messages"].clone();
    assert_eq!(
        messages,
        json!([
            {"role": "system", "content": "first"},
            {"role": "system", "content": "second"},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgpB"}},
            ]},
            {"role": "assistant", "content": "seen"},
            {"role": "user", "content": [
                {"type": "text", "text": "again"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgpC"}},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgpB"}},
            ]},
            {"role": "user", "content": "continue"},
        ])
    );
}

#[test]
fn chat_completions_without_instructions_starts_with_the_image_message() {
    let images = CapturedImages::new();
    let image = images.capture(1, "only.png", b"\x89PNG\r\n\x1a\nA");
    let request = OwnedRequest {
        instructions: Vec::new(),
        ..image_request(vec![user_with_images("look", vec![image])])
    };

    let body = build(&request, ToolChoiceMode::Omit).unwrap();

    assert!(
        body.contains(r#""messages":[{"role":"user","content":[{"type":"text","text":"look"},"#),
        "{body}"
    );
}

#[test]
fn chat_completions_requires_each_image_snapshot_to_verify() {
    let images = CapturedImages::new();
    let corrupted = images.capture(1, "corrupted.png", b"\x89PNG\r\n\x1a\nA");
    fs::write(
        corrupted.snapshot_path.as_deref().unwrap(),
        b"\x89PNG\r\n\x1a\nB",
    )
    .unwrap();
    let missing = images.capture(2, "missing.png", b"\x89PNG\r\n\x1a\nC");
    fs::remove_file(missing.snapshot_path.as_deref().unwrap()).unwrap();
    let uncaptured = ImageAttachment {
        id: 3,
        path: "/tmp/never-captured.png".to_owned(),
        media_type: "image/png".to_owned(),
        ..ImageAttachment::default()
    };

    for image in [corrupted, missing, uncaptured] {
        let request = image_request(vec![user_with_images("look", vec![image])]);
        assert_eq!(
            build(&request, ToolChoiceMode::Omit),
            Err(ProtocolError::ImageUnavailable)
        );
    }
}
