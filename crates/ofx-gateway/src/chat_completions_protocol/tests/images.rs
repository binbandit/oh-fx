use std::fs;

use ofx_contract::ToolImage;

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

fn tool_with_images(
    id: &str,
    content: &str,
    status: ToolResultStatus,
    images: Vec<ToolImage>,
) -> ChatMessage {
    ChatMessage::Tool {
        call_id: ToolCallId::new(id),
        tool_name: "read_file".to_owned(),
        content: content.to_owned(),
        status,
        images,
    }
}

fn tool_image(data: &str, mime_type: &str) -> ToolImage {
    ToolImage {
        data: data.to_owned(),
        mime_type: mime_type.to_owned(),
        source_ref: None,
    }
}

fn calls(ids: &[&str]) -> ChatMessage {
    ChatMessage::Assistant {
        content: None,
        tool_calls: ids.iter().map(|id| call(id, "read_file", "{}")).collect(),
        provider_replay: None,
    }
}

fn sent_messages(request: &OwnedRequest) -> Vec<Value> {
    let body: Value = serde_json::from_str(&build(request, ToolChoiceMode::Omit).unwrap()).unwrap();
    body["messages"].as_array().unwrap().clone()
}

#[test]
fn chat_completions_serializes_retained_tool_images_as_a_follow_up_user_message() {
    let request = OwnedRequest {
        messages: vec![
            ChatMessage::user("hi"),
            calls(&["call-1"]),
            tool_with_images(
                "call-1",
                "image attached",
                ToolResultStatus::Success,
                vec![tool_image("aGVsbG8", "image/png")],
            ),
        ],
        ..test_tool_request()
    };

    let body = build(&request, ToolChoiceMode::Omit).unwrap();

    assert!(
        body.contains(concat!(
            r#"{"role":"tool","content":"image attached","tool_call_id":"call-1"},"#,
            r#"{"role":"user","content":[{"type":"text","text":"The tool \"read_file\" returned 1 image(s)."},"#,
            r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8"}}]}],"#,
        )),
        "{body}"
    );
    let messages = sent_messages(&request);
    assert_eq!(
        messages[messages.len() - 1],
        json!({"role": "user", "content": [
            {"type": "text", "text": "The tool \"read_file\" returned 1 image(s)."},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8"}},
        ]})
    );
    assert_eq!(messages[messages.len() - 2]["content"], "image attached");
}

#[test]
fn chat_completions_merges_parallel_tool_images_into_one_follow_up_after_the_tool_run() {
    let request = OwnedRequest {
        messages: vec![
            ChatMessage::user("hi"),
            calls(&["call-1", "call-2"]),
            tool_with_images(
                "call-1",
                "image a",
                ToolResultStatus::Success,
                vec![tool_image("aGVsbG8", "image/png")],
            ),
            tool_with_images(
                "call-2",
                "image b",
                ToolResultStatus::Success,
                vec![tool_image("d29ybGQ", "image/jpeg")],
            ),
            ChatMessage::user("thanks"),
        ],
        ..test_tool_request()
    };

    let messages = sent_messages(&request);

    assert_eq!(messages.len(), 8);
    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[5]["role"], "tool");
    assert_eq!(
        messages[6],
        json!({"role": "user", "content": [
            {"type": "text", "text": "Tool results returned 2 image(s)."},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8"}},
            {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,d29ybGQ"}},
        ]})
    );
    assert_eq!(messages[7], json!({"role": "user", "content": "thanks"}));
}

#[test]
fn chat_completions_withholds_images_from_denied_tool_results() {
    let request = OwnedRequest {
        messages: vec![
            ChatMessage::user("hi"),
            calls(&["call-1"]),
            tool_with_images(
                "call-1",
                r#"{"error":{"type":"tool_permission_denied","reason":"user_denied"}}"#,
                ToolResultStatus::Failure,
                vec![tool_image("aGVsbG8", "image/png")],
            ),
        ],
        ..test_tool_request()
    };

    let messages = sent_messages(&request);

    assert_eq!(messages.len(), 5);
    assert!(
        !build(&request, ToolChoiceMode::Omit)
            .unwrap()
            .contains("image_url")
    );
}
