use super::*;
use crate::json_fields::parse_json;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
const GIF: &[u8] = b"GIF89a\x01\0\x01\0";

fn png_data() -> String {
    STANDARD.encode(PNG)
}

fn parsed(text: &str) -> Result<Vec<ToolImage>, SavedImagesError> {
    let value = parse_json(text.as_bytes()).unwrap();
    parse_saved_tool_images(value.as_array().unwrap())
}

fn image(data: &str, mime_type: &str) -> String {
    format!("{{\"type\":\"image\",\"mimeType\":\"{mime_type}\",\"data\":\"{data}\"}}")
}

#[test]
fn source_references_are_bounded_opaque_text_without_control_bytes() {
    assert!(valid_source_ref("host:screenshot-1"));
    assert!(valid_source_ref(&"é".repeat(256)));
    for value in ["", &"a".repeat(513), "bad\nref", "bad\u{7f}ref"] {
        assert!(!valid_source_ref(value), "{value:?}");
    }
}

#[test]
fn inline_image_bytes_must_sniff_as_their_declared_media_type() {
    assert!(is_valid_inline_image(PNG, "image/png"));
    assert!(is_valid_inline_image(GIF, "image/gif"));
    assert!(!is_valid_inline_image(PNG, "image/gif"));
    assert!(!is_valid_inline_image(b"", "image/png"));
    assert!(!is_valid_inline_image(b"not an image", "image/png"));
    let webp = b"RIFF\0\0\0\0WEBPVP8 ";
    assert!(is_valid_inline_image(webp, "image/webp"));
    assert!(is_valid_inline_image(
        &[0xff, 0xd8, 0xff, 0xe0],
        "image/jpeg"
    ));
    assert!(!is_valid_inline_image(
        &b"RIFF\0\0\0\0WEB"[..],
        "image/webp"
    ));
}

#[test]
fn saved_tool_images_read_upstreams_spelling_with_source_references() {
    let data = png_data();
    let images = parsed(&format!(
        "[{},{{\"type\":\"image\",\"mimeType\":\"image/png\",\"data\":\"{data}\",\"source_ref\":\"host:a\"}},{{\"type\":\"image\",\"mimeType\":\"image/gif\",\"source_ref\":\"host:b\"}}]",
        image(&data, "image/png")
    ))
    .unwrap();
    assert_eq!(
        images,
        [
            ToolImage {
                data: data.clone(),
                mime_type: "image/png".to_owned(),
                source_ref: None,
            },
            ToolImage {
                data,
                mime_type: "image/png".to_owned(),
                source_ref: Some("host:a".to_owned()),
            },
            ToolImage {
                data: String::new(),
                mime_type: "image/gif".to_owned(),
                source_ref: Some("host:b".to_owned()),
            },
        ]
    );
}

#[test]
fn a_bad_source_reference_refuses_the_saved_images() {
    let data = png_data();
    for text in [
        format!(
            "[{{\"type\":\"image\",\"mimeType\":\"image/png\",\"data\":\"{data}\",\"sourceRef\":\"host:a\"}}]"
        ),
        format!(
            "[{{\"type\":\"image\",\"mimeType\":\"image/png\",\"data\":\"{data}\",\"source_ref\":\"bad\\nref\"}}]"
        ),
        format!(
            "[{{\"type\":\"image\",\"mimeType\":\"image/png\",\"data\":\"{data}\",\"source_ref\":null}}]"
        ),
    ] {
        assert_eq!(
            parsed(&text),
            Err(SavedImagesError::InvalidSourceRef),
            "{text}"
        );
    }
}

#[test]
fn image_checks_name_the_failure_upstream_reports() {
    let data = png_data();
    let nine = vec![image(&data, "image/png"); 9].join(",");
    let large = "A".repeat(MAX_ENCODED_IMAGE_BYTES + 4);
    for (text, error) in [
        (format!("[{nine}]"), ImageError::ImageLimitExceeded),
        (
            format!("[{}]", image(&data, "image/gif")),
            ImageError::InvalidImage,
        ),
        (
            format!("[{}]", image("!!!!", "image/png")),
            ImageError::InvalidImage,
        ),
        (
            format!("[{}]", image(&STANDARD.encode(b"plain text"), "image/png")),
            ImageError::UnsupportedImageType,
        ),
        (
            format!("[{}]", image("", "image/png")),
            ImageError::ImageLimitExceeded,
        ),
        (
            format!("[{}]", image(&large, "image/png")),
            ImageError::ImageLimitExceeded,
        ),
        ("[1]".to_owned(), ImageError::InvalidImage),
        (
            "[{\"type\":\"image\",\"mimeType\":\"image/png\"}]".to_owned(),
            ImageError::InvalidImage,
        ),
        (
            format!("[{{\"type\":\"image\",\"data\":\"{data}\"}}]"),
            ImageError::InvalidImage,
        ),
        (
            "[{\"type\":\"image\",\"mimeType\":\"image/bmp\",\"source_ref\":\"host:a\"}]"
                .to_owned(),
            ImageError::InvalidImage,
        ),
    ] {
        assert_eq!(parsed(&text), Err(SavedImagesError::Image(error)), "{text}");
    }
}

#[test]
fn unsupported_blocks_are_skipped_and_embedded_resources_are_read() {
    let data = png_data();
    let images = parsed(&format!(
        "[{{\"type\":\"text\",\"text\":\"x\"}},{},{{\"type\":\"resource\",\"resource\":{{\"mimeType\":\"image/png\",\"blob\":\"{data}\"}}}},{{\"type\":\"resource\",\"resource\":{{\"blob\":\"{data}\"}}}}]",
        image(&data, "image/bmp")
    ))
    .unwrap();
    assert_eq!(
        images,
        [ToolImage {
            data,
            mime_type: "image/png".to_owned(),
            source_ref: None,
        }]
    );
}

#[test]
fn image_names_match_upstreams_error_names() {
    assert_eq!(ImageError::InvalidImage.name(), "InvalidImage");
    assert_eq!(ImageError::ImageLimitExceeded.name(), "ImageLimitExceeded");
    assert_eq!(
        ImageError::UnsupportedImageType.name(),
        "UnsupportedImageType"
    );
}
