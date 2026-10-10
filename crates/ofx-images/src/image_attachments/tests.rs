use std::fs;
use std::path::Path;
use std::process::Command;

use super::*;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap()
}

fn write(directory: &Path, name: &str, bytes: &[u8]) -> String {
    let path = directory.join(name);
    fs::write(&path, bytes).unwrap();
    canonical(&path)
}

#[test]
fn load_accepts_files_larger_than_the_header_length() {
    let directory = tempfile::tempdir().unwrap();
    let path = write(
        directory.path(),
        "large.png",
        &[PNG_SIGNATURE, &[b'a'; 256]].concat(),
    );

    let attachment = load_resolved_image_attachment(path.clone()).unwrap();

    assert_eq!(
        attachment,
        ImageAttachment {
            path,
            media_type: "image/png".to_owned(),
            ..ImageAttachment::default()
        }
    );
}

#[test]
fn load_detects_each_supported_type_from_its_bytes() {
    let directory = tempfile::tempdir().unwrap();
    for (name, bytes, media_type) in [
        ("photo.jpg", &b"\xff\xd8\xffrest"[..], "image/jpeg"),
        ("anim.gif", b"GIF89arest", "image/gif"),
        ("still.webp", b"RIFFxxxxWEBPrest", "image/webp"),
        ("named.gif", b"\x89PNG\r\n\x1a\nrest", "image/png"),
    ] {
        let path = write(directory.path(), name, bytes);
        let attachment = load_resolved_image_attachment(path).unwrap();
        assert_eq!(attachment.media_type, media_type, "{name}");
    }
}

#[test]
fn image_header_short_reads_return_only_the_bytes_read() {
    let header = read_image_header(&mut &b"short"[..], 64).unwrap();

    assert_eq!(header, b"short");
}

#[test]
fn image_header_reads_at_most_its_limit() {
    let bytes = [7_u8; 100];

    assert_eq!(read_image_header(&mut &bytes[..], 100).unwrap().len(), 64);
    assert_eq!(read_image_header(&mut &bytes[..], 10).unwrap().len(), 10);
}

#[test]
fn image_attachment_rejects_directories_before_header_reads() {
    let directory = tempfile::tempdir().unwrap();

    assert_eq!(
        load_resolved_image_attachment(canonical(directory.path())),
        Err(AttachmentError::NotRegularFile)
    );
}

#[test]
fn image_attachment_rejects_a_fifo_without_waiting_for_a_writer() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("pipe.png");
    let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success());

    assert_eq!(
        load_resolved_image_attachment(canonical(&fifo)),
        Err(AttachmentError::NotRegularFile)
    );
}

#[test]
fn image_attachment_rejects_missing_files() {
    let directory = tempfile::tempdir().unwrap();
    let missing = format!("{}/missing.png", canonical(directory.path()));

    assert_eq!(
        load_resolved_image_attachment(missing),
        Err(AttachmentError::FileNotFound)
    );
}

#[test]
fn image_attachment_rejects_unsupported_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = write(directory.path(), "notes.png", b"plain text, not an image");

    assert_eq!(
        load_resolved_image_attachment(path),
        Err(AttachmentError::UnsupportedImageType)
    );
}

#[test]
fn image_attachment_checks_the_size_limit_before_the_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let at_limit = write(directory.path(), "limit.png", PNG_SIGNATURE);
    File::options()
        .write(true)
        .open(&at_limit)
        .unwrap()
        .set_len(MAX_IMAGE_BYTES)
        .unwrap();
    let over_limit = write(directory.path(), "over.txt", b"text");
    File::options()
        .write(true)
        .open(&over_limit)
        .unwrap()
        .set_len(MAX_IMAGE_BYTES + 1)
        .unwrap();

    assert_eq!(
        load_resolved_image_attachment(at_limit).map(|image| image.media_type),
        Ok("image/png".to_owned())
    );
    assert_eq!(
        load_resolved_image_attachment(over_limit),
        Err(AttachmentError::ImageTooLarge)
    );
}

#[test]
fn normalize_path_input_unescapes_finder_style_spaces() {
    assert_eq!(
        normalize_path_input("/Users/me/CleanShot\\ 2026-04-08\\ at\\ 12.16.27.png"),
        "/Users/me/CleanShot 2026-04-08 at 12.16.27.png"
    );
}

#[test]
fn normalize_path_input_trims_and_strips_balanced_outer_quotes() {
    for (input, expected) in [
        (" \t\"/tmp/a b.png\"\r\n", "/tmp/a b.png"),
        ("'/tmp/a b.png'", "/tmp/a b.png"),
        ("\"/tmp/a.png'", "\"/tmp/a.png'"),
        ("\"", "\""),
        ("\"\"", ""),
        ("trailing\\", "trailing\\"),
        ("a\\\\b", "a\\b"),
        ("caf\\é.png", "café.png"),
    ] {
        assert_eq!(normalize_path_input(input), expected, "{input:?}");
    }
}
