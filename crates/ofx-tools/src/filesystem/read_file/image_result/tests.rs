use std::fs::{self, File};
use std::path::PathBuf;

use ofx_contract::PathAccess;

use super::super::ReadFileArgs;
use super::*;
use crate::filesystem::FilesystemContext;

struct Workspace {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        fs::write(self.root.join(name), bytes).unwrap();
    }

    fn read(&self, name: &str) -> ImageRead {
        ReadFileArgs::decode(&format!(r#"{{"path":"{name}"}}"#))
            .unwrap()
            .read(
                &FilesystemContext::new(&self.root),
                &PathAccess::WorkspaceOrExternal,
            )
            .unwrap()
    }
}

fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes
}

fn jpeg(width: u16, height: u16) -> Vec<u8> {
    [
        &b"\xff\xd8\xff\xc0\x00\x11\x08"[..],
        &height.to_be_bytes(),
        &width.to_be_bytes(),
        b"\x03\x01\x22\x00\x02\x11\x01\x03\x11\x01\xff\xd9",
    ]
    .concat()
}

fn not_attached(name: &str, mime_type: &str, size: u64, reason: &str) -> String {
    format!(
        "<path>{name}</path>\n<content>image not attached: {mime_type} ({size} bytes); {reason}. Use an available image tool to save a smaller copy to a new file, then read_file the copy. If no image tool is available, ask the user before installing one.</content>"
    )
}

#[test]
fn read_file_attaches_a_png_within_the_single_request_limit_unchanged() {
    let workspace = Workspace::new();
    let bytes = png_header(3420, 2224);
    workspace.write("frame.png", &bytes);

    let read = workspace.read("frame.png");

    assert_eq!(
        read.text,
        "<path>frame.png</path>\n<content>image attached (image/png, 24 bytes)</content>"
    );
    assert!(read.covered);
    assert_eq!(
        read.images,
        [ToolImage {
            data: STANDARD.encode(&bytes),
            mime_type: "image/png".to_owned(),
            source_ref: None,
        }]
    );
}

#[test]
fn read_file_attaches_a_jpeg_that_fits_the_single_request_limit() {
    let workspace = Workspace::new();
    workspace.write("photo.jpg", &jpeg(4032, 3024));

    let read = workspace.read("photo.jpg");

    assert!(
        read.text.contains("image attached (image/jpeg"),
        "{}",
        read.text
    );
    assert_eq!(read.images.len(), 1);
    let decoded = STANDARD.decode(&read.images[0].data).unwrap();
    assert_eq!(
        image_dimensions(&decoded).map(|size| (size.width, size.height)),
        Some((4032, 3024))
    );
}

#[test]
fn read_file_attaches_an_image_exactly_at_the_model_pixel_limit() {
    let workspace = Workspace::new();
    workspace.write("edge.png", &png_header(8000, 8000));

    assert_eq!(workspace.read("edge.png").images.len(), 1);
}

#[test]
fn read_file_gives_an_actionable_path_for_images_it_cannot_attach() {
    let workspace = Workspace::new();
    workspace.write("huge.png", &png_header(8001, 1));
    workspace.write("unknown.png", b"\x89PNG\r\n\x1a\nrest");
    let mut padded = png_header(64, 8);
    padded.resize(MAX_ENCODED_IMAGE_BYTES / 4 * 3 + 1, 0);
    workspace.write("padded.png", &padded);
    let big = workspace.root.join("big.png");
    fs::write(&big, png_header(10, 10)).unwrap();
    File::options()
        .write(true)
        .open(&big)
        .unwrap()
        .set_len(10 * 1024 * 1024 + 1)
        .unwrap();

    for (name, size, reason) in [
        ("huge.png", 24, "the image exceeds 8000 pixels per side"),
        (
            "unknown.png",
            12,
            "the image dimensions could not be verified",
        ),
        (
            "padded.png",
            padded.len() as u64,
            "the image exceeds the 5 MiB encoded attach limit",
        ),
        (
            "big.png",
            10 * 1024 * 1024 + 1,
            "the file exceeds read_file's 10 MiB read limit",
        ),
    ] {
        let read = workspace.read(name);
        assert_eq!(read.text, not_attached(name, "image/png", size, reason));
        assert!(!read.covered, "{name}");
        assert!(read.images.is_empty(), "{name}");
    }
}

#[test]
fn read_file_reports_attached_images_on_its_tool_output() {
    let workspace = Workspace::new();
    workspace.write("frame.png", &png_header(2, 3));

    let output = ReadFileArgs::decode(r#"{"path":"frame.png"}"#)
        .unwrap()
        .run(
            &FilesystemContext::new(&workspace.root),
            &PathAccess::WorkspaceOrExternal,
        );

    assert_eq!(
        output.content,
        "<path>frame.png</path>\n<content>image attached (image/png, 24 bytes)</content>"
    );
    assert_eq!(output.images().len(), 1);
    assert_eq!(output.images()[0].mime_type, "image/png");
    assert_eq!(output.model_view_covers_full_file, Some(true));
}
