use std::cell::Cell;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::*;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const MAX_ENCODED_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn path(&self, name: &str) -> String {
        self.root.join(name).into_os_string().into_string().unwrap()
    }

    fn write(&self, name: &str, bytes: &[u8]) -> String {
        fs::write(self.root.join(name), bytes).unwrap();
        self.path(name)
    }

    fn snapshot_dir(&self) -> String {
        self.path("snapshots")
    }

    fn attachment(&self, id: u64, name: &str, bytes: &[u8]) -> ImageAttachment {
        ImageAttachment {
            id,
            path: self.write(name, bytes),
            media_type: "image/png".to_owned(),
            ..ImageAttachment::default()
        }
    }

    fn captured(&self, name: &str, bytes: &[u8]) -> ImageAttachment {
        let mut attachments = [self.attachment(1, name, bytes)];
        capture_image_snapshots(&mut attachments, &self.snapshot_dir(), UNLIMITED).unwrap();
        let [attachment] = attachments;
        attachment
    }
}

const UNLIMITED: CaptureBudget<'static> = &|| Ok(());

fn count_files(directory: &str) -> usize {
    fs::read_dir(directory).map_or(0, |entries| {
        entries
            .filter(|entry| {
                let kind = entry.as_ref().unwrap().file_type().unwrap();
                kind.is_file() || kind.is_symlink()
            })
            .count()
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes))
}

fn snapshot(path: &str, bytes: &[u8]) -> ImageAttachment {
    ImageAttachment {
        id: 1,
        path: "/tmp/original.png".to_owned(),
        media_type: "image/png".to_owned(),
        snapshot_path: Some(path.to_owned()),
        snapshot_sha256: Some(sha256_hex(bytes)),
        ..ImageAttachment::default()
    }
}

fn set_len(path: &str, length: u64) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(length)
        .unwrap();
}

#[test]
fn snapshot_directory_handle_syncs_after_create_and_after_reopen() {
    let fixture = Fixture::new();

    let created = open_or_create_snapshot_directory(&fixture.snapshot_dir()).unwrap();
    sync_directory(&created).unwrap();
    let reopened = open_or_create_snapshot_directory(&fixture.snapshot_dir()).unwrap();
    sync_directory(&reopened).unwrap();

    let mode = fs::metadata(fixture.snapshot_dir())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn capture_writes_an_immutable_private_snapshot_named_by_id_and_digest() {
    let fixture = Fixture::new();
    let bytes = [PNG, b"captured"].concat();

    let attachment = fixture.captured("source.png", &bytes);

    let digest = sha256_hex(&bytes);
    let expected = format!("{}/image-1-{}.bin", fixture.snapshot_dir(), &digest[..16]);
    assert_eq!(attachment.snapshot_path.as_deref(), Some(expected.as_str()));
    assert_eq!(attachment.snapshot_sha256.as_deref(), Some(digest.as_str()));
    assert_eq!(attachment.media_type, "image/png");
    assert_eq!(fs::read(&expected).unwrap(), bytes);
    let mode = fs::metadata(&expected).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(count_files(&fixture.snapshot_dir()), 1);
}

#[test]
fn capture_rejects_source_limit_plus_one_without_snapshot_residue() {
    let fixture = Fixture::new();
    let mut attachments = [fixture.attachment(1, "too-large.png", PNG)];
    set_len(&attachments[0].path, MAX_IMAGE_BYTES + 1);

    assert_eq!(
        capture_image_snapshots(&mut attachments, &fixture.snapshot_dir(), UNLIMITED),
        Err(AttachmentError::ImageTooLarge)
    );
    assert_eq!(count_files(&fixture.snapshot_dir()), 0);
}

#[test]
fn capture_rejects_a_zero_image_id() {
    let fixture = Fixture::new();
    let mut attachments = [fixture.attachment(0, "zero.png", PNG)];

    assert_eq!(
        capture_image_snapshots(&mut attachments, &fixture.snapshot_dir(), UNLIMITED),
        Err(AttachmentError::InvalidImageId)
    );
}

#[test]
fn cancelled_capture_removes_every_partial_artifact() {
    let fixture = Fixture::new();
    let mut attachments = [fixture.attachment(1, "cancel.png", &[PNG, b"cancel"].concat())];
    let checks = Cell::new(0);
    let budget = || {
        checks.set(checks.get() + 1);
        if checks.get() == 3 {
            Err(AttachmentError::Cancelled)
        } else {
            Ok(())
        }
    };

    assert_eq!(
        capture_image_snapshots(&mut attachments, &fixture.snapshot_dir(), &budget),
        Err(AttachmentError::Cancelled)
    );
    assert_eq!(checks.get(), 3);
    assert_eq!(count_files(&fixture.snapshot_dir()), 0);
    assert_eq!(attachments[0].snapshot_path, None);
}

#[test]
fn capture_preserves_a_source_at_the_exact_encoded_byte_limit() {
    let fixture = Fixture::new();
    let largest_fitting_raw_image = MAX_ENCODED_IMAGE_BYTES / 4 * 3;
    let mut attachments = [fixture.attachment(1, "exact.png", PNG)];
    set_len(&attachments[0].path, largest_fitting_raw_image);

    capture_image_snapshots(&mut attachments, &fixture.snapshot_dir(), UNLIMITED).unwrap();

    let snapshot = attachments[0].snapshot_path.as_deref().unwrap();
    assert_eq!(
        fs::metadata(snapshot).unwrap().len(),
        largest_fitting_raw_image
    );
}

#[test]
fn capture_rejects_a_file_that_grows_past_the_source_limit_while_streaming() {
    let fixture = Fixture::new();
    let mut attachments = [fixture.attachment(1, "growing.png", PNG)];
    let path = attachments[0].path.clone();
    let checks = Cell::new(0);
    let budget = || {
        checks.set(checks.get() + 1);
        if checks.get() == 2 {
            set_len(&path, MAX_IMAGE_BYTES + 1);
        }
        Ok(())
    };

    assert_eq!(
        capture_image_snapshots(&mut attachments, &fixture.snapshot_dir(), &budget),
        Err(AttachmentError::ImageTooLarge)
    );
    assert_eq!(count_files(&fixture.snapshot_dir()), 0);
}

#[test]
fn capture_derives_the_media_type_from_the_captured_bytes() {
    let fixture = Fixture::new();
    let mut attachments = [ImageAttachment {
        media_type: "image/jpeg".to_owned(),
        ..fixture.attachment(1, "mislabeled.jpg", b"\xff\xd8\xfforiginal-jpeg")
    }];
    let path = attachments[0].path.clone();
    let checks = Cell::new(0);
    let budget = || {
        checks.set(checks.get() + 1);
        if checks.get() == 2 {
            fs::write(&path, [PNG, b"captured"].concat()).unwrap();
        }
        Ok(())
    };

    capture_image_snapshots(&mut attachments, &fixture.snapshot_dir(), &budget).unwrap();

    assert_eq!(attachments[0].media_type, "image/png");
}

#[test]
fn capture_rejects_bytes_without_an_image_signature_and_leaves_no_file() {
    let fixture = Fixture::new();
    let mut attachments = [fixture.attachment(1, "text.png", b"plain text")];

    assert_eq!(
        capture_image_snapshots(&mut attachments, &fixture.snapshot_dir(), UNLIMITED),
        Err(AttachmentError::UnsupportedImageType)
    );
    assert_eq!(count_files(&fixture.snapshot_dir()), 0);
}

#[test]
fn partial_multi_image_capture_rolls_back_earlier_snapshots() {
    let fixture = Fixture::new();
    let mut attachments = [
        fixture.attachment(1, "first.png", &[PNG, b"first"].concat()),
        ImageAttachment {
            id: 2,
            path: fixture.path("missing.png"),
            media_type: "image/png".to_owned(),
            ..ImageAttachment::default()
        },
    ];

    assert_eq!(
        capture_image_snapshots(&mut attachments, &fixture.snapshot_dir(), UNLIMITED),
        Err(AttachmentError::FileNotFound)
    );
    assert_eq!(count_files(&fixture.snapshot_dir()), 0);
    for attachment in &attachments {
        assert_eq!(attachment.snapshot_path, None);
        assert_eq!(attachment.snapshot_sha256, None);
    }
}

#[test]
fn twenty_image_capture_retains_one_immutable_snapshot_per_image() {
    let fixture = Fixture::new();
    let mut attachments: Vec<ImageAttachment> = (1..=20)
        .map(|id| fixture.attachment(id, &format!("image-{id}.png"), &[PNG, b"many"].concat()))
        .collect();

    capture_image_snapshots(&mut attachments, &fixture.snapshot_dir(), UNLIMITED).unwrap();

    assert_eq!(count_files(&fixture.snapshot_dir()), 20);
}

#[test]
fn snapshot_capture_rejects_a_symlinked_destination_directory() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("session")).unwrap();
    fs::create_dir(fixture.root.join("outside")).unwrap();
    fs::write(fixture.root.join("outside/sentinel"), "unchanged").unwrap();
    symlink(
        fixture.root.join("outside"),
        fixture.root.join("session/images"),
    )
    .unwrap();
    let mut attachments = [fixture.attachment(1, "source.png", &[PNG, b"source"].concat())];

    assert_eq!(
        capture_image_snapshots(&mut attachments, &fixture.path("session/images"), UNLIMITED),
        Err(AttachmentError::ImageSnapshotPathUnsafe)
    );
    assert_eq!(attachments[0].snapshot_path, None);
    assert_eq!(attachments[0].snapshot_sha256, None);
    assert_eq!(count_files(&fixture.path("outside")), 1);
}

#[test]
fn snapshot_capture_rejects_relative_and_dotted_directories() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("session")).unwrap();
    for directory in [
        "relative/images".to_owned(),
        format!("{}/./images", fixture.path("session")),
        format!("{}/../session/images", fixture.path("session")),
        format!("{}/.", fixture.path("session")),
        "/".to_owned(),
    ] {
        let mut attachments = [fixture.attachment(1, "source.png", PNG)];
        assert_eq!(
            capture_image_snapshots(&mut attachments, &directory, UNLIMITED),
            Err(AttachmentError::ImageSnapshotPathUnsafe),
            "{directory}"
        );
    }
}

#[test]
fn snapshot_discard_does_not_follow_a_replaced_destination_directory() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("session")).unwrap();
    fs::create_dir(fixture.root.join("outside")).unwrap();
    let mut attachments = [fixture.attachment(1, "source.png", &[PNG, b"source"].concat())];
    capture_image_snapshots(&mut attachments, &fixture.path("session/images"), UNLIMITED).unwrap();
    let snapshot = attachments[0].snapshot_path.clone().unwrap();
    let leaf = Path::new(&snapshot).file_name().unwrap().to_owned();
    fs::rename(
        fixture.root.join("session/images"),
        fixture.root.join("session/images-owned"),
    )
    .unwrap();
    fs::write(fixture.root.join("outside").join(&leaf), "outside-sentinel").unwrap();
    symlink(
        fixture.root.join("outside"),
        fixture.root.join("session/images"),
    )
    .unwrap();

    discard_image_snapshots(&mut attachments);

    let sentinel = fs::read_to_string(fixture.root.join("outside").join(&leaf)).unwrap();
    assert_eq!(sentinel, "outside-sentinel");
    assert_eq!(attachments[0].snapshot_path, None);
}

#[test]
fn discarding_shared_snapshots_deletes_each_file_once() {
    let fixture = Fixture::new();
    let captured = fixture.captured("source.png", &[PNG, b"shared"].concat());
    let path = captured.snapshot_path.clone().unwrap();
    let mut attachments = [captured.clone(), captured];

    discard_image_snapshots(&mut attachments);

    assert!(!Path::new(&path).exists());
    assert!(
        attachments
            .iter()
            .all(|image| image.snapshot_path.is_none())
    );
}

#[test]
fn verified_snapshot_loading_rejects_a_symlink_before_and_after_retargeting() {
    let fixture = Fixture::new();
    let original = [PNG, b"symlink-a"].concat();
    let path_a = fixture.write("a.bin", &original);
    let path_b = fixture.write("b.bin", &[PNG, b"symlink-b"].concat());
    let link = fixture.path("snapshot-link.bin");
    symlink(&path_a, &link).unwrap();
    let attachment = snapshot(&link, &original);

    assert_eq!(
        load_verified_snapshot(&attachment, UNLIMITED),
        Err(AttachmentError::ImageSnapshotPathUnsafe)
    );
    fs::remove_file(&link).unwrap();
    symlink(&path_b, &link).unwrap();
    assert_eq!(
        load_verified_snapshot(&attachment, UNLIMITED),
        Err(AttachmentError::ImageSnapshotPathUnsafe)
    );
}

#[test]
fn verified_snapshot_loading_rejects_a_symlinked_directory() {
    let fixture = Fixture::new();
    let original = [PNG, b"symlink-directory"].concat();
    fs::create_dir(fixture.root.join("owned")).unwrap();
    fs::write(fixture.root.join("owned/snapshot.bin"), &original).unwrap();
    symlink(fixture.root.join("owned"), fixture.root.join("images-link")).unwrap();
    let attachment = snapshot(&fixture.path("images-link/snapshot.bin"), &original);

    assert_eq!(
        load_verified_snapshot(&attachment, UNLIMITED),
        Err(AttachmentError::ImageSnapshotPathUnsafe)
    );
}

#[test]
fn verified_snapshot_loading_reads_the_opened_handle_after_path_replacement() {
    let fixture = Fixture::new();
    let original = [PNG, b"opened-handle"].concat();
    let replacement = [PNG, b"replacement"].concat();
    let path = fixture.write("snapshot.bin", &original);
    let replacement_path = fixture.write("replacement.bin", &replacement);
    let attachment = snapshot(&path, &original);
    let checks = Cell::new(0);
    let budget = || {
        checks.set(checks.get() + 1);
        if checks.get() == 2 {
            fs::rename(&replacement_path, &path).unwrap();
        }
        Ok(())
    };

    let verified = load_verified_snapshot(&attachment, &budget).unwrap();

    assert_eq!(verified.bytes, original);
    assert_eq!(fs::read(&path).unwrap(), replacement);
}

#[test]
fn verified_snapshot_loading_keeps_the_bytes_after_the_source_is_deleted() {
    let fixture = Fixture::new();
    let png = [PNG, b"image-a"].concat();
    let attachment = fixture.captured("image.png", &png);
    fs::remove_file(&attachment.path).unwrap();

    let verified = load_verified_snapshot(&attachment, UNLIMITED).unwrap();

    assert_eq!(
        verified,
        VerifiedSnapshot {
            bytes: png,
            media_type: "image/png",
        }
    );
}

#[test]
fn verified_snapshot_loading_rejects_a_missing_snapshot_without_source_fallback() {
    let fixture = Fixture::new();
    let attachment = fixture.captured("image.png", &[PNG, b"image-a"].concat());
    fs::remove_file(attachment.snapshot_path.as_deref().unwrap()).unwrap();

    assert_eq!(
        load_verified_snapshot(&attachment, UNLIMITED),
        Err(AttachmentError::FileNotFound)
    );
}

#[test]
fn verified_snapshot_loading_rejects_a_modified_snapshot_digest() {
    let fixture = Fixture::new();
    let attachment = fixture.captured("image.png", &[PNG, b"image-a"].concat());
    fs::write(
        attachment.snapshot_path.as_deref().unwrap(),
        [PNG, b"image-b"].concat(),
    )
    .unwrap();

    assert_eq!(
        load_verified_snapshot(&attachment, UNLIMITED),
        Err(AttachmentError::ImageSnapshotCorrupt)
    );
}

#[test]
fn verified_snapshot_loading_checks_the_recorded_snapshot_fields() {
    let fixture = Fixture::new();
    let png = [PNG, b"fields"].concat();
    let path = fixture.write("snapshot.bin", &png);
    let linked = fixture.path("linked.bin");
    fs::hard_link(fixture.write("other.bin", &png), &linked).unwrap();
    for (attachment, error) in [
        (
            ImageAttachment {
                snapshot_path: None,
                ..snapshot(&path, &png)
            },
            AttachmentError::MissingImageSnapshot,
        ),
        (
            ImageAttachment {
                snapshot_sha256: None,
                ..snapshot(&path, &png)
            },
            AttachmentError::MissingImageSnapshot,
        ),
        (
            ImageAttachment {
                snapshot_sha256: Some("abc".to_owned()),
                ..snapshot(&path, &png)
            },
            AttachmentError::InvalidImageSnapshotDigest,
        ),
        (
            ImageAttachment {
                snapshot_sha256: Some(sha256_hex(&png).to_uppercase()),
                ..snapshot(&path, &png)
            },
            AttachmentError::ImageSnapshotCorrupt,
        ),
        (
            ImageAttachment {
                media_type: "image/gif".to_owned(),
                ..snapshot(&path, &png)
            },
            AttachmentError::ImageSnapshotMediaTypeMismatch,
        ),
        (
            snapshot(&fixture.path(""), &png),
            AttachmentError::ImageSnapshotPathUnsafe,
        ),
        (
            snapshot("relative/snapshot.bin", &png),
            AttachmentError::ImageSnapshotPathUnsafe,
        ),
        (snapshot(&linked, &png), AttachmentError::NotRegularFile),
    ] {
        assert_eq!(
            load_verified_snapshot(&attachment, UNLIMITED),
            Err(error),
            "{attachment:?}"
        );
    }
}

#[test]
fn verified_snapshot_loading_rejects_a_snapshot_over_the_source_limit() {
    let fixture = Fixture::new();
    let path = fixture.write("large.bin", PNG);
    set_len(&path, MAX_IMAGE_BYTES + 1);

    assert_eq!(
        load_verified_snapshot(&snapshot(&path, PNG), UNLIMITED),
        Err(AttachmentError::ImageTooLarge)
    );
}

#[test]
fn verified_inline_bytes_are_checked_like_a_snapshot_file() {
    let png = [PNG, b"inline"].concat();
    let inline = ImageAttachment {
        snapshot_path: None,
        inline_data: Some(png.clone()),
        ..snapshot("/unused", &png)
    };

    assert_eq!(
        load_verified_snapshot(&inline, UNLIMITED).map(|verified| verified.bytes),
        Ok(png.clone())
    );
    let corrupted = ImageAttachment {
        inline_data: Some([PNG, b"changed"].concat()),
        ..inline.clone()
    };
    assert_eq!(
        load_verified_snapshot(&corrupted, UNLIMITED),
        Err(AttachmentError::ImageSnapshotCorrupt)
    );
    let undigested = ImageAttachment {
        snapshot_sha256: None,
        ..inline
    };
    assert_eq!(
        load_verified_snapshot(&undigested, UNLIMITED),
        Err(AttachmentError::MissingImageSnapshot)
    );
}

#[test]
fn verified_snapshot_loading_stops_when_the_budget_does() {
    let fixture = Fixture::new();
    let attachment = fixture.captured("image.png", &[PNG, b"budget"].concat());

    assert_eq!(
        load_verified_snapshot(&attachment, &|| Err(AttachmentError::Cancelled)),
        Err(AttachmentError::Cancelled)
    );
}

#[test]
fn temporary_snapshot_directories_are_private_and_removed_whole() {
    let directory = create_temp_snapshot_dir().unwrap();
    let root = fs::canonicalize(TEMP_ROOT).unwrap();
    let name = Path::new(&directory).file_name().unwrap().to_str().unwrap();

    assert_eq!(Path::new(&directory).parent(), Some(root.as_path()));
    assert!(name.starts_with(TEMP_DIRECTORY_PREFIX));
    let mode = fs::metadata(&directory).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    let mut attachments = [ImageAttachment {
        id: 1,
        path: {
            let source = format!("{directory}/source.png");
            fs::write(&source, [PNG, b"temp"].concat()).unwrap();
            source
        },
        media_type: "image/png".to_owned(),
        ..ImageAttachment::default()
    }];
    capture_image_snapshots(&mut attachments, &format!("{directory}/images"), UNLIMITED).unwrap();
    cleanup_snapshot_dir(&directory);
    assert!(!Path::new(&directory).exists());
}
