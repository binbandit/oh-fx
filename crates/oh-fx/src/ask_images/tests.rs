use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use super::*;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path())
        .unwrap()
        .join("workspace");
    fs::create_dir(&root).unwrap();
    (directory, root)
}

fn load(root: &Path, input: &str) -> Result<ImageAttachment, ImageLoadError> {
    load_user_image_attachment(root, OsStr::new(input))
}

#[test]
fn load_user_image_attachment_resolves_paths_from_the_workspace() {
    let (_directory, root) = workspace();
    fs::write(root.join("photo.png"), PNG).unwrap();
    let expected = root
        .join("photo.png")
        .into_os_string()
        .into_string()
        .unwrap();

    let attachment = load(&root, "photo.png").unwrap();

    assert_eq!(attachment.path, expected);
    assert_eq!(attachment.media_type, "image/png");
    assert_eq!(attachment.id, 0);
    assert_eq!(attachment.snapshot_path, None);
}

#[test]
fn load_user_image_attachment_reads_quoted_escaped_and_external_paths() {
    let (directory, root) = workspace();
    fs::write(root.join("Clean Shot.png"), PNG).unwrap();
    let outside = fs::canonicalize(directory.path())
        .unwrap()
        .join("outside.gif");
    fs::write(&outside, b"GIF89arest").unwrap();
    let outside = outside.into_os_string().into_string().unwrap();

    for input in [
        "Clean\\ Shot.png",
        "\"Clean Shot.png\"",
        " 'Clean Shot.png'\n",
        "./Clean Shot.png",
    ] {
        let attachment = load(&root, input).unwrap();
        assert!(
            attachment.path.ends_with("/workspace/Clean Shot.png"),
            "{input:?}"
        );
    }
    assert_eq!(load(&root, &outside).unwrap().media_type, "image/gif");
    assert_eq!(load(&root, "../outside.gif").unwrap().path, outside);
}

#[test]
fn load_user_image_attachment_names_upstream_failures() {
    let (_directory, root) = workspace();
    fs::write(root.join("notes.png"), b"plain text").unwrap();
    fs::create_dir(root.join("folder.png")).unwrap();

    for (input, error, reason) in [
        (
            "missing.png",
            ImageLoadError::Path(PathError::FileNotFound),
            "image file not found",
        ),
        (
            "notes.png",
            ImageLoadError::Image(AttachmentError::UnsupportedImageType),
            "unsupported image type",
        ),
        (
            "folder.png",
            ImageLoadError::Image(AttachmentError::NotRegularFile),
            "NotRegularFile",
        ),
        (
            "  ",
            ImageLoadError::Path(PathError::InvalidPath),
            "InvalidPath",
        ),
        (
            "~user/a.png",
            ImageLoadError::Path(PathError::InvalidPath),
            "InvalidPath",
        ),
    ] {
        assert_eq!(load(&root, input), Err(error), "{input:?}");
        assert_eq!(error.reason(), reason, "{input:?}");
    }
    assert_eq!(
        ImageLoadError::Image(AttachmentError::ImageTooLarge).reason(),
        "image exceeds the 20 MiB limit"
    );
    assert_eq!(
        ImageLoadError::Image(AttachmentError::FileNotFound).reason(),
        "image file not found"
    );
    assert_eq!(
        ImageLoadError::Image(AttachmentError::ImageTooLarge).to_string(),
        "ImageTooLarge"
    );
}

#[test]
fn load_user_image_attachment_rejects_paths_that_are_not_utf8() {
    let (_directory, root) = workspace();

    assert_eq!(
        load_user_image_attachment(&root, OsStr::from_bytes(b"\xff.png")),
        Err(ImageLoadError::Path(PathError::InvalidPath))
    );
}
