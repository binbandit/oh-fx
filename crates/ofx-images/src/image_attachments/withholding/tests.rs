use std::fs;

use super::*;
use crate::image_data::tests::{test_jpeg, test_jpeg_behind_metadata, test_png_header};

const MAX_IMAGE_DIMENSION: u32 = 2000;

fn user(content: &str, images: Vec<ImageAttachment>) -> ChatMessage {
    ChatMessage::User {
        content: content.to_owned(),
        restored_steering: false,
        feedback_for: None,
        images,
    }
}

fn parts(message: &ChatMessage) -> (&str, &[ImageAttachment]) {
    match message {
        ChatMessage::User {
            content, images, ..
        } => (content, images),
        _ => panic!("expected a user message"),
    }
}

struct Snapshots {
    _directory: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Snapshots {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.root.join(name);
        fs::write(&path, bytes).unwrap();
        path.into_os_string().into_string().unwrap()
    }
}

fn snapshot(id: u64, path: &str, media_type: &str) -> ImageAttachment {
    ImageAttachment {
        id,
        path: format!("original-{id}"),
        media_type: media_type.to_owned(),
        snapshot_path: Some(path.to_owned()),
        ..ImageAttachment::default()
    }
}

#[test]
fn requests_leave_out_attachments_over_the_model_pixel_limit_and_name_their_saved_file() {
    let snapshots = Snapshots::new();
    let wide = snapshots.write("image-1-0000000000000001.bin", &test_jpeg(3420, 2224));
    let small = snapshots.write("image-2-0000000000000002.bin", &test_png_header(10, 10));
    let messages = [
        user(
            "compare [Image #1] and [Image #2]",
            vec![
                snapshot(1, &wide, "image/jpeg"),
                snapshot(2, &small, "image/png"),
            ],
        ),
        ChatMessage::Assistant {
            content: Some("ok".to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        },
    ];

    let projected = AttachmentDimensionCache::default()
        .withhold(&messages, MAX_IMAGE_DIMENSION)
        .unwrap();

    let (content, images) = parts(&projected[0]);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].id, 2);
    assert_eq!(
        content,
        format!(
            "[Image #1 not sent: image/jpeg is 3420x2224 pixels. This request permits at most 2000 per side and 5 MiB encoded per image. The original is saved at {wide}. Use an available image tool to save a smaller copy to a new file ending in .jpg, then read_file the copy. If no image tool is available, ask the user before installing one.]\ncompare [Image #1] and [Image #2]"
        )
    );
    assert_eq!(projected[1], messages[1]);
    assert_eq!(parts(&messages[0]).1.len(), 2);
}

#[test]
fn request_cache_still_withholds_a_snapshot_deleted_after_dimension_lookup() {
    let snapshots = Snapshots::new();
    let wide = snapshots.write("image-1-0000000000000001.bin", &test_jpeg(3420, 2224));
    let messages = [
        user(
            "[Image #1]",
            vec![ImageAttachment {
                snapshot_sha256: Some("a".repeat(64)),
                ..snapshot(1, &wide, "image/jpeg")
            }],
        ),
        ChatMessage::user("and now?"),
    ];
    let cache = AttachmentDimensionCache::default();

    let first = cache.withhold(&messages, MAX_IMAGE_DIMENSION).unwrap();
    fs::remove_file(&wide).unwrap();
    let second = cache.withhold(&messages, MAX_IMAGE_DIMENSION).unwrap();

    assert!(parts(&first[0]).1.is_empty());
    assert!(parts(&second[0]).1.is_empty());
    assert_eq!(parts(&first[0]).0, parts(&second[0]).0);
}

#[test]
fn requests_ask_for_a_smaller_copy_of_an_oversized_in_memory_attachment() {
    let messages = [user(
        "[Image #3]",
        vec![ImageAttachment {
            id: 3,
            path: "inline://image-3".to_owned(),
            media_type: "image/jpeg".to_owned(),
            inline_data: Some(test_jpeg(2400, 10)),
            ..ImageAttachment::default()
        }],
    )];

    let projected = AttachmentDimensionCache::default()
        .withhold(&messages, MAX_IMAGE_DIMENSION)
        .unwrap();

    assert_eq!(
        parts(&projected[0]),
        (
            "[Image #3 not sent: image/jpeg is 2400x10 pixels. This request permits at most 2000 per side and 5 MiB encoded per image. No local image file is available; ask the user for a smaller copy.]\n[Image #3]",
            &[][..]
        )
    );
}

#[test]
fn requests_keep_verified_attachments_within_the_pixel_limit_unchanged() {
    let snapshots = Snapshots::new();
    let edge = test_png_header(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION);
    let path = snapshots.write("image-2-0000000000000002.bin", &edge);
    let messages = [user(
        "[Image #1] [Image #2]",
        vec![
            ImageAttachment {
                id: 1,
                path: "inline://image-1".to_owned(),
                media_type: "image/png".to_owned(),
                inline_data: Some(edge),
                ..ImageAttachment::default()
            },
            snapshot(2, &path, "image/png"),
        ],
    )];

    assert_eq!(
        AttachmentDimensionCache::default().withhold(&messages, MAX_IMAGE_DIMENSION),
        None
    );
}

#[test]
fn reference_only_attachments_get_host_recovery_guidance_without_attempting_file_access() {
    let messages = [user(
        "",
        vec![ImageAttachment {
            id: 1,
            path: "inline://image-1".to_owned(),
            media_type: "image/png".to_owned(),
            source_ref: Some("host:original".to_owned()),
            ..ImageAttachment::default()
        }],
    )];

    let projected = AttachmentDimensionCache::default()
        .withhold(&messages, 8000)
        .unwrap();

    let (notice, images) = parts(&projected[0]);
    assert!(images.is_empty());
    assert!(notice.contains("only a host source reference was supplied"));
    assert!(notice.contains("host:original"));
    assert!(notice.contains("host-provided tool"));
    assert!(!notice.contains("read_file"));
    assert_eq!(parts(&messages[0]).1.len(), 1);
}

#[test]
fn requests_withhold_snapshots_whose_dimensions_cannot_be_verified() {
    let messages = [user(
        "",
        vec![snapshot(2, "/nonexistent/image-2.bin", "image/png")],
    )];

    let projected = AttachmentDimensionCache::default()
        .withhold(&messages, 8000)
        .unwrap();

    let (notice, images) = parts(&projected[0]);
    assert!(images.is_empty());
    assert_eq!(
        notice,
        "[Image #2 not sent: its dimensions could not be verified. The original is saved at /nonexistent/image-2.bin. Use an available image tool to save a smaller copy to a new file ending in .png, then read_file the copy. If no image tool is available, ask the user before installing one.]\n"
    );
}

#[test]
fn requests_find_a_jpeg_frame_header_behind_large_metadata() {
    let snapshots = Snapshots::new();
    let path = snapshots.write(
        "image-1-0000000000000001.bin",
        &test_jpeg_behind_metadata(4032, 3024),
    );
    let messages = [user("[Image #1]", vec![snapshot(1, &path, "image/jpeg")])];

    let projected = AttachmentDimensionCache::default()
        .withhold(&messages, MAX_IMAGE_DIMENSION)
        .unwrap();

    assert!(
        parts(&projected[0])
            .0
            .starts_with("[Image #1 not sent: image/jpeg is 4032x3024 pixels.")
    );
}

#[test]
fn requests_withhold_attachments_over_the_encoded_byte_limit_or_shared_by_links() {
    let snapshots = Snapshots::new();
    let large = snapshots.write("image-1-0000000000000001.bin", &test_png_header(10, 10));
    File::options()
        .write(true)
        .open(&large)
        .unwrap()
        .set_len(5 * 1024 * 1024 / 4 * 3 + 1)
        .unwrap();
    let linked = snapshots.write("image-2-0000000000000002.bin", &test_png_header(10, 10));
    fs::hard_link(&linked, snapshots.root.join("second-name.bin")).unwrap();
    let messages = [user(
        "[Image #1] [Image #2]",
        vec![
            snapshot(1, &large, "image/png"),
            snapshot(2, &linked, "image/png"),
        ],
    )];

    let projected = AttachmentDimensionCache::default()
        .withhold(&messages, 8000)
        .unwrap();

    let (notice, images) = parts(&projected[0]);
    assert!(images.is_empty());
    assert!(notice.starts_with("[Image #1 not sent: image/png is 10x10 pixels."));
    assert!(notice.contains("[Image #2 not sent: image/png is 10x10 pixels."));
}

#[test]
fn more_than_twenty_request_images_lower_the_pixel_limit() {
    let snapshots = Snapshots::new();
    let path = snapshots.write("image-1-0000000000000001.bin", &test_png_header(3000, 10));
    let images = (1..=21)
        .map(|id| snapshot(id, &path, "image/png"))
        .collect();
    let messages = [user("many", images)];
    let cache = AttachmentDimensionCache::default();

    assert!(cache.withhold_oversized_attachments(&messages).is_some());
    assert_eq!(cache.withhold_oversized_attachments(&messages[..0]), None);
    let twenty = [user("many", parts(&messages[0]).1[..20].to_vec())];
    assert_eq!(cache.withhold_oversized_attachments(&twenty), None);
}
