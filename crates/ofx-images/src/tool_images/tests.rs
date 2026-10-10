use serde_json::json;

use super::*;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jP0cAAAAASUVORK5CYII=";

fn parse(content: &Value) -> Result<Vec<ToolImage>, ImageError> {
    parse_tool_images(content.as_array().expect("test content is an array"))
}

#[test]
fn source_references_are_bounded_opaque_utf8_without_control_bytes() {
    assert!(valid_source_ref("host:screenshot-1"));
    assert!(valid_source_ref(&"é".repeat(256)));
    for value in ["", &"a".repeat(513), "bad\nref", "bad\x7fref"] {
        assert!(!valid_source_ref(value), "{value:?}");
    }
}

#[test]
fn tool_source_references_support_deferred_images_without_weakening_validation() {
    let images =
        parse(&json!([{"type":"image","mimeType":"image/png","sourceRef":"host:original"}]))
            .expect("a deferred image parses");
    assert_eq!(
        images,
        [ToolImage {
            data: String::new(),
            mime_type: "image/png".to_owned(),
            source_ref: Some("host:original".to_owned()),
        }]
    );
    let mut list = ImageList::default();
    assert_eq!(
        list.append("", "image/png"),
        Err(ImageError::ImageLimitExceeded)
    );
    assert_eq!(
        list.append_with_source_ref("", "image/png", Some("bad\nref")),
        Err(ImageError::InvalidImage)
    );
}

#[test]
fn tool_images_validate_data_and_declared_media_type() {
    assert_eq!(validate_image(PNG, "image/png"), Ok(()));
    assert_eq!(
        validate_image(PNG, "image/jpeg"),
        Err(ImageError::InvalidImage)
    );
    assert_eq!(
        validate_image("not base64", "image/png"),
        Err(ImageError::InvalidImage)
    );
}

#[test]
fn images_and_embedded_image_resources_are_kept_in_order() {
    let images = parse(&json!([
        {"type":"text","text":"before"},
        {"type":"image","data":PNG,"mimeType":"image/png"},
        {"type":"resource","resource":{"uri":"file:///a.png","blob":PNG,"mimeType":"image/png","sourceRef":"ignored"}},
        {"type":"image","data":PNG,"mimeType":"image/png","sourceRef":"host:shot"}
    ]))
    .expect("valid images parse");
    let refs: Vec<_> = images
        .iter()
        .map(|image| image.source_ref.as_deref())
        .collect();
    assert_eq!(refs, [None, None, Some("host:shot")]);
    assert!(
        images
            .iter()
            .all(|image| image.data == PNG && image.mime_type == "image/png")
    );
}

#[test]
fn unsupported_and_incomplete_blocks_are_skipped() {
    let images = parse(&json!([
        "text",
        {"type":7},
        {"type":"audio","data":"AA==","mimeType":"audio/wav"},
        {"type":"image","data":"AA==","mimeType":"image/svg+xml"},
        {"type":"resource"},
        {"type":"resource","resource":"file:///a"},
        {"type":"resource","resource":{"uri":"file:///a","text":"body"}},
        {"type":"resource","resource":{"uri":"file:///a","blob":"AA=="}},
        {"type":"resource","resource":{"uri":"file:///a","blob":"AA==","mimeType":"application/pdf"}}
    ]))
    .expect("nothing to attach");
    assert!(images.is_empty());
}

#[test]
fn malformed_image_blocks_fail_the_whole_result() {
    for content in [
        json!([{"type":"image","mimeType":"image/png"}]),
        json!([{"type":"image","data":PNG}]),
        json!([{"type":"image","data":7,"mimeType":"image/png"}]),
        json!([{"type":"image","data":PNG,"mimeType":null}]),
        json!([{"type":"image","data":PNG,"mimeType":"image/png","sourceRef":7}]),
        json!([{"type":"image","data":PNG,"mimeType":"image/png","sourceRef":""}]),
        json!([{"type":"image","mimeType":"image/svg+xml","sourceRef":"host:a"}]),
        json!([{"type":"image","data":PNG,"mimeType":"image/gif"}]),
        json!([{"type":"resource","resource":{"blob":7,"mimeType":"image/png"}}]),
        json!([{"type":"resource","resource":{"blob":PNG,"mimeType":["image/png"]}}]),
    ] {
        assert_eq!(parse(&content), Err(ImageError::InvalidImage), "{content}");
    }
}

#[test]
fn image_data_that_is_not_an_image_or_empty_is_refused() {
    assert_eq!(
        parse(&json!([{"type":"image","data":"aGk=","mimeType":"image/png"}])),
        Err(ImageError::UnsupportedImageType)
    );
    assert_eq!(
        parse(&json!([{"type":"resource","resource":{"blob":"","mimeType":"image/png"}}])),
        Err(ImageError::ImageLimitExceeded)
    );
}

#[test]
fn a_result_carries_at_most_eight_images() {
    let image = json!({"type":"image","data":PNG,"mimeType":"image/png"});
    let eight = vec![image.clone(); MAX_TOOL_IMAGES];
    assert_eq!(
        parse(&json!(eight)).map(|images| images.len()),
        Ok(MAX_TOOL_IMAGES)
    );
    let nine = vec![image; MAX_TOOL_IMAGES + 1];
    assert_eq!(parse(&json!(nine)), Err(ImageError::ImageLimitExceeded));
}

#[test]
fn encoded_images_are_bounded_one_by_one_and_together() {
    let mut list = ImageList::default();
    let oversized = "A".repeat(MAX_ENCODED_IMAGE_BYTES + 1);
    assert_eq!(
        list.append(&oversized, "image/png"),
        Err(ImageError::ImageLimitExceeded)
    );
    list.encoded_bytes = MAX_RESULT_FRAME_BYTES - PNG.len() + 1;
    assert_eq!(
        list.append(PNG, "image/png"),
        Err(ImageError::ImageLimitExceeded)
    );
    list.encoded_bytes -= 1;
    assert_eq!(list.append(PNG, "image/png"), Ok(()));
    assert_eq!(list.encoded_bytes, MAX_RESULT_FRAME_BYTES);
}
