use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

const SOURCES: &[(&str, &str)] = &[
    ("src/builtins/system_prompt.md", "system\n"),
    (
        "src/core/compactor/summarize.zig",
        "pub const system_prompt =\n    \"notes\";\n",
    ),
    (
        "src/core/permissions/auto_classifier.zig",
        "const review_policy_template =\n    \\\\<review>\n    \\\\{{REVIEW_DATA}}\n    \\\\</review>\n    \\\\\n;\n",
    ),
];
const GOLDENS: &[(&str, &str)] = &[
    ("system_prompt.md", "system\n"),
    ("compaction_system_prompt.txt", "notes"),
    (
        "review-policy/review_policy.xml",
        "<review>\n{{REVIEW_DATA}}\n</review>\n",
    ),
];

fn setup_git(directory: &Path, args: &[&str]) -> String {
    super::super::git(directory, args).unwrap()
}
fn commit(directory: &Path) -> String {
    setup_git(directory, &["add", "-A"]);
    setup_git(
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
            "test: golden fixture",
        ],
    );
    setup_git(directory, &["rev-parse", "HEAD"])
        .trim()
        .to_owned()
}
struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    upstream: PathBuf,
    pin: String,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let root = base.join("root");
        let upstream = base.join("upstream");
        std::fs::create_dir_all(root.join("parity/goldens")).unwrap();
        for (path, content) in SOURCES {
            let path = upstream.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        setup_git(&upstream, &["init", "-q"]);
        let pin = commit(&upstream);
        std::fs::write(root.join("parity/UPSTREAM"), &pin).unwrap();
        let fixture = Self {
            _directory: directory,
            root,
            upstream,
            pin,
        };
        fixture.write_goldens("previous fixture");
        fixture
    }
    fn destination(&self, name: &str) -> PathBuf {
        self.root.join("parity/goldens").join(name)
    }
    fn write_goldens(&self, value: &str) {
        for (name, _) in GOLDENS {
            let path = self.destination(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, value).unwrap();
        }
    }
    fn assert_previous(&self) {
        for (name, _) in GOLDENS {
            assert_eq!(
                std::fs::read(self.destination(name)).unwrap(),
                b"previous fixture",
                "{name}"
            );
        }
    }
}

#[test]
fn shared_regeneration_uses_pinned_objects_and_all_extractors() {
    let fixture = Fixture::new();
    for (path, _) in SOURCES {
        std::fs::write(fixture.upstream.join(path), "changed HEAD").unwrap();
    }
    commit(&fixture.upstream);
    for (path, _) in SOURCES {
        std::fs::write(fixture.upstream.join(path), "dirty working file").unwrap();
    }
    regenerate(&fixture.root, &fixture.upstream).unwrap();
    for (name, expected) in GOLDENS {
        assert_eq!(
            std::fs::read(fixture.destination(name)).unwrap(),
            expected.as_bytes()
        );
    }
    check(&fixture.root, &fixture.upstream).unwrap();
}
#[test]
fn check_rejects_stale_missing_and_changed_pin_without_writes() {
    let fixture = Fixture::new();
    assert!(check(&fixture.root, &fixture.upstream).is_err());
    fixture.assert_previous();
    std::fs::remove_dir_all(fixture.root.join("parity/goldens")).unwrap();
    assert!(check(&fixture.root, &fixture.upstream).is_err());
    assert!(!fixture.root.join("parity/goldens").exists());
    regenerate(&fixture.root, &fixture.upstream).unwrap();
    std::fs::write(fixture.upstream.join(SOURCES[0].0), "new pin content").unwrap();
    let pin = commit(&fixture.upstream);
    std::fs::write(fixture.root.join("parity/UPSTREAM"), pin).unwrap();
    assert!(check(&fixture.root, &fixture.upstream).is_err());
    assert_eq!(
        std::fs::read(fixture.destination(GOLDENS[0].0)).unwrap(),
        GOLDENS[0].1.as_bytes()
    );
}
#[test]
fn invalid_commit_or_late_extraction_preserves_every_fixture() {
    let fixture = Fixture::new();
    for pin in [
        "invalid".to_owned(),
        "0".repeat(40),
        setup_git(&fixture.upstream, &["rev-parse", "HEAD^{tree}"]),
    ] {
        std::fs::write(fixture.root.join("parity/UPSTREAM"), pin).unwrap();
        assert!(regenerate(&fixture.root, &fixture.upstream).is_err());
        fixture.assert_previous();
    }
    std::fs::write(fixture.upstream.join(SOURCES[2].0), "unsupported policy").unwrap();
    let pin = commit(&fixture.upstream);
    std::fs::write(fixture.root.join("parity/UPSTREAM"), pin).unwrap();
    assert!(regenerate(&fixture.root, &fixture.upstream).is_err());
    fixture.assert_previous();
}
#[test]
fn canonicalize_error_names_the_requested_path() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent-upstream");
    let error = run(&["--upstream", absent.to_str().unwrap()]).unwrap_err();
    assert!(error.contains(absent.to_str().unwrap()), "{error}");
}

fn object_files(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(object_files(&path));
        } else {
            files.insert(path.clone(), std::fs::read(path).unwrap());
        }
    }
    files
}

#[test]
fn missing_promisor_sources_never_contact_remote_or_mutate_objects_or_fixtures() {
    for (missing, _) in SOURCES {
        let fixture = Fixture::new();
        let base = fixture.root.parent().unwrap();
        let donor = base.join("donor");
        setup_git(
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
            setup_git(&fixture.upstream, &["config", key, &value]);
        }
        let blob = setup_git(
            &fixture.upstream,
            &["rev-parse", &format!("{}:{missing}", fixture.pin)],
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
        for checking in [false, true] {
            let result = if checking {
                check(&fixture.root, &fixture.upstream)
            } else {
                regenerate(&fixture.root, &fixture.upstream)
            };
            assert!(!marker.exists(), "contacted remote for {missing}");
            assert!(result.is_err(), "accepted missing {missing}");
            assert_eq!(object_files(&fixture.upstream.join(".git/objects")), before);
            fixture.assert_previous();
        }
    }
}

#[test]
fn check_preserves_matching_fixture_metadata_and_objects() {
    let fixture = Fixture::new();
    regenerate(&fixture.root, &fixture.upstream).unwrap();
    let files = object_files(&fixture.root.join("parity/goldens"));
    let objects = object_files(&fixture.upstream.join(".git/objects"));
    let modified: Vec<_> = GOLDENS
        .iter()
        .map(|(name, _)| {
            std::fs::metadata(fixture.destination(name))
                .unwrap()
                .modified()
                .unwrap()
        })
        .collect();
    check(&fixture.root, &fixture.upstream).unwrap();
    assert_eq!(object_files(&fixture.root.join("parity/goldens")), files);
    assert_eq!(
        object_files(&fixture.upstream.join(".git/objects")),
        objects
    );
    let after: Vec<_> = GOLDENS
        .iter()
        .map(|(name, _)| {
            std::fs::metadata(fixture.destination(name))
                .unwrap()
                .modified()
                .unwrap()
        })
        .collect();
    assert_eq!(after, modified);
}

#[test]
fn portable_offline_command_uses_environment_lockdown_without_new_git_option() {
    if std::env::var_os("OH_FX_GOLDEN_GIT_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "parity::goldens::tests::portable_offline_command_uses_environment_lockdown_without_new_git_option"])
            .env("OH_FX_GOLDEN_GIT_CHILD", "1")
            .env("GIT_NO_LAZY_FETCH", "0")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_DIR", "hostile-directory")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "remote.origin.url")
            .env("GIT_CONFIG_VALUE_0", "hostile-url")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let stub = directory.path().join("git-2.39-stub");
    std::fs::write(&stub, r#"#!/bin/sh
for arg in "$@"; do if [ "$arg" = --no-lazy-fetch ]; then exit 64; fi; done
if [ -n "${GIT_DIR+x}${GIT_CONFIG_COUNT+x}${GIT_CONFIG_KEY_0+x}${GIT_CONFIG_VALUE_0+x}" ]; then exit 65; fi
printf '%s:%s' "$GIT_NO_LAZY_FETCH" "$GIT_ALLOW_PROTOCOL"
"#).unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = super::super::git_command(
        stub.as_os_str(),
        directory.path(),
        &["rev-parse", "--verify", "HEAD^{commit}"],
        true,
    );
    for variable in [
        "GIT_DIR",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
    ] {
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == std::ffi::OsStr::new(variable))
                .unwrap()
                .1,
            None
        );
    }
    let output = command.output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"1:");
    let environment: BTreeMap<_, _> = command.get_envs().collect();
    assert_eq!(
        environment.get(std::ffi::OsStr::new("GIT_NO_LAZY_FETCH")),
        Some(&Some(std::ffi::OsStr::new("1")))
    );
    assert_eq!(
        environment.get(std::ffi::OsStr::new("GIT_ALLOW_PROTOCOL")),
        Some(&Some(std::ffi::OsStr::new("")))
    );
}

#[test]
fn leaf_extractors_retain_strict_prompt_grammar() {
    assert_eq!(
        compaction::extract("pub const system_prompt =\n    \"one \" ++\n    \"two.\";\n").unwrap(),
        "one two."
    );
    for source in [
        "",
        "pub const system_prompt =\n    other;",
        "pub const system_prompt =\n    \"escape\\n\";",
        "pub const system_prompt =\n    \"unterminated\" ++",
        "pub const system_prompt =\n    \"a\";\npub const system_prompt =\n    \"b\";",
    ] {
        assert!(compaction::extract(source).is_err());
    }
    assert_eq!(
        review_policy::extract(
            "const review_policy_template =\n    \\\\one\n    \\\\  two\n    \\\\\n;\n"
        )
        .unwrap(),
        "one\n  two\n"
    );
    for source in [
        "",
        "const review_policy_template =\n    other;",
        "const review_policy_template =\n    \\\\a\n",
        "const review_policy_template =\n    \\\\a\n;\nconst review_policy_template =\n    \\\\b\n;",
        "const review_policy_template =\n    \\\\a\n;",
        "const review_policy_template =\n    \\\\\n;",
    ] {
        assert!(review_policy::extract(source).is_err());
    }
    assert!(run(&["--unknown"]).is_err());
    assert!(run(&["--check", "--check"]).is_err());
}
