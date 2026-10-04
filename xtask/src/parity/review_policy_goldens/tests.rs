use super::*;

fn git(directory: &Path, args: &[&str]) -> String {
    super::super::git(directory, args).unwrap()
}

fn commit(directory: &Path) -> String {
    git(directory, &["add", "-A"]);
    git(
        directory,
        &[
            "-c",
            "user.name=Parity Fixture",
            "-c",
            "user.email=parity@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "test: review policy fixture",
        ],
    );
    git(directory, &["rev-parse", "HEAD"])
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: std::path::PathBuf,
    upstream: std::path::PathBuf,
    pin: String,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let root = base.join("root");
        let upstream = base.join("upstream");
        std::fs::create_dir_all(root.join("parity/goldens/review-policy")).unwrap();
        std::fs::create_dir_all(upstream.join("src/core/permissions")).unwrap();
        std::fs::write(upstream.join("src/core/permissions/auto_classifier.zig"), "const review_policy_template =\n    \\\\<review>\n    \\\\{{REVIEW_DATA}}\n    \\\\</review>\n    \\\\\n;\n").unwrap();
        git(&upstream, &["init", "-q"]);
        let pin = commit(&upstream);
        std::fs::write(root.join("parity/UPSTREAM"), &pin).unwrap();
        std::fs::write(
            root.join("parity/goldens/review-policy/review_policy.xml"),
            "previous fixture",
        )
        .unwrap();
        Self {
            _directory: directory,
            root,
            upstream,
            pin,
        }
    }

    fn golden(&self) -> Vec<u8> {
        std::fs::read(
            self.root
                .join("parity/goldens/review-policy/review_policy.xml"),
        )
        .unwrap()
    }
}

#[test]
fn multiline_extraction_preserves_spaces_blank_lines_and_final_newline() {
    assert_eq!(
        extract("const review_policy_template =\n    \\\\one\n    \\\\  two\n    \\\\\n;\n")
            .unwrap(),
        "one\n  two\n"
    );
}

#[test]
fn multiline_extraction_rejects_missing_duplicate_changed_or_unterminated_declarations() {
    for source in [
        "",
        "const review_policy_template =\n    other;",
        "const review_policy_template =\n    \\\\a\n",
        "const review_policy_template =\n    \\\\a\n;\nconst review_policy_template =\n    \\\\b\n;",
        "const review_policy_template =\n    \\\\a\n;\n",
        "const review_policy_template =\n    \\\\\n;\n",
    ] {
        assert!(extract(source).is_err(), "{source}");
    }
    assert!(run(&["--unknown"]).is_err());
}

#[test]
fn regeneration_uses_pinned_objects_ignores_working_files_and_is_idempotent() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture
            .upstream
            .join("src/core/permissions/auto_classifier.zig"),
        "changed source",
    )
    .unwrap();
    commit(&fixture.upstream);
    regenerate(&fixture.root, &fixture.upstream).unwrap();
    assert_eq!(fixture.golden(), b"<review>\n{{REVIEW_DATA}}\n</review>\n");
    regenerate(&fixture.root, &fixture.upstream).unwrap();
    assert_eq!(fixture.golden(), b"<review>\n{{REVIEW_DATA}}\n</review>\n");
}

#[test]
fn invalid_pin_missing_objects_and_changed_extraction_preserve_existing_fixture() {
    let fixture = Fixture::new();
    for pin in ["invalid", "0000000000000000000000000000000000000000"] {
        std::fs::write(fixture.root.join("parity/UPSTREAM"), pin).unwrap();
        assert!(regenerate(&fixture.root, &fixture.upstream).is_err());
        assert_eq!(fixture.golden(), b"previous fixture");
    }
    std::fs::write(
        fixture
            .upstream
            .join("src/core/permissions/auto_classifier.zig"),
        "changed declaration",
    )
    .unwrap();
    let pin = commit(&fixture.upstream);
    std::fs::write(fixture.root.join("parity/UPSTREAM"), pin).unwrap();
    assert!(regenerate(&fixture.root, &fixture.upstream).is_err());
    assert_eq!(fixture.golden(), b"previous fixture");
}

fn object_files(directory: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(object_files(&path));
        } else {
            let content = std::fs::read(&path).unwrap();
            files.insert(path, content);
        }
    }
    files
}

#[test]
fn missing_promisor_blob_never_contacts_remote_changes_objects_or_replaces_fixture() {
    let fixture = Fixture::new();
    let base = fixture.root.parent().unwrap();
    let donor = base.join("donor");
    git(
        base,
        &[
            "clone",
            "--bare",
            "--no-hardlinks",
            fixture.upstream.to_str().unwrap(),
            donor.to_str().unwrap(),
        ],
    );
    let marker = base.join("contacted");
    let script = base.join("upload-pack");
    std::fs::write(
        &script,
        format!(
            "printf contacted > '{}'\nexec git-upload-pack \"$@\"\n",
            marker.display()
        ),
    )
    .unwrap();
    for (key, value) in [
        ("remote.origin.url", donor.to_str().unwrap().to_owned()),
        ("remote.origin.promisor", "true".to_owned()),
        ("remote.origin.partialclonefilter", "blob:none".to_owned()),
        (
            "remote.origin.uploadpack",
            format!("sh '{}'", script.display()),
        ),
    ] {
        git(&fixture.upstream, &["config", key, &value]);
    }
    let blob = git(
        &fixture.upstream,
        &[
            "rev-parse",
            &format!(
                "{}:src/core/permissions/auto_classifier.zig",
                fixture.pin.trim()
            ),
        ],
    );
    let blob = blob.trim();
    std::fs::remove_file(
        fixture
            .upstream
            .join(".git/objects")
            .join(&blob[..2])
            .join(&blob[2..]),
    )
    .unwrap();
    let before = object_files(&fixture.upstream.join(".git/objects"));
    assert!(regenerate(&fixture.root, &fixture.upstream).is_err());
    assert!(!marker.exists());
    assert_eq!(object_files(&fixture.upstream.join(".git/objects")), before);
    assert_eq!(fixture.golden(), b"previous fixture");
}
