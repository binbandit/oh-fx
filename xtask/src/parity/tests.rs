use super::*;
use std::process::{Command, Output};

struct Fixture {
    root: tempfile::TempDir,
    upstream: tempfile::TempDir,
    pin: String,
}

fn fixture_git(directory: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    command.current_dir(directory);
    for (variable, _) in std::env::vars_os() {
        if variable.to_string_lossy().starts_with("GIT_") {
            command.env_remove(variable);
        }
    }
    command.args([
        "-c",
        "user.name=Parity Fixture",
        "-c",
        "user.email=parity@example.invalid",
        "-c",
        "commit.gpgsign=false",
    ]);
    command.args(args).output().unwrap()
}

impl Fixture {
    fn new() -> Self {
        let mut fixture = Self {
            root: tempfile::tempdir().unwrap(),
            upstream: tempfile::tempdir().unwrap(),
            pin: String::new(),
        };
        fs::create_dir_all(fixture.root.path().join("parity/files")).unwrap();
        fs::create_dir_all(fixture.root.path().join("docs")).unwrap();
        fs::create_dir_all(fixture.upstream.path().join("src")).unwrap();
        fs::write(fixture.upstream.path().join("src/a.zig"), "").unwrap();
        fs::create_dir_all(fixture.root.path().join("crates/example/src")).unwrap();
        fs::write(fixture.root.path().join("crates/example/src/lib.rs"), "").unwrap();
        assert!(
            fixture_git(fixture.upstream.path(), &["init", "-q"])
                .status
                .success()
        );
        fixture.commit_sources();
        fixture
    }

    fn commit_sources(&mut self) {
        assert!(
            fixture_git(self.upstream.path(), &["add", "-A"])
                .status
                .success()
        );
        assert!(
            fixture_git(
                self.upstream.path(),
                &["commit", "--allow-empty", "-qm", "test: fixture"]
            )
            .status
            .success()
        );
        self.pin =
            String::from_utf8(fixture_git(self.upstream.path(), &["rev-parse", "HEAD"]).stdout)
                .unwrap()
                .trim()
                .to_owned();
        fs::write(self.root.path().join("parity/UPSTREAM"), &self.pin).unwrap();
        fs::write(
            self.root.path().join("docs/upstream-parity.md"),
            format!("- **Sync point:** `{}`\n", &self.pin[..7]),
        )
        .unwrap();
    }

    fn map(&self, rows: &str) {
        fs::write(self.root.path().join("parity/files/example.toml"), rows).unwrap();
    }

    fn check(&self) -> Result<Counts, String> {
        check(self.root.path(), self.upstream.path())
    }
}

fn row(source: &str, status: &str, modules: &str, note: &str) -> String {
    format!("[[file]]\nupstream = '{source}'\nstatus = '{status}'\nmodules = [{modules}]\n{note}\n")
}

#[test]
fn complete_map_counts_each_status() {
    let mut fixture = Fixture::new();
    let mut rows = row("src/a.zig", "todo", "", "");
    for (index, status) in ["todo", "partial", "ported", "not-applicable"]
        .iter()
        .enumerate()
    {
        let source = format!("src/{index}.zig");
        fs::write(fixture.upstream.path().join(&source), "").unwrap();
        rows.push_str(&row(
            &source,
            status,
            "'crates/example/src/lib.rs'",
            "note = 'concrete reason'",
        ));
    }
    fixture.commit_sources();
    fixture.map(&rows);
    assert_eq!(
        fixture.check().unwrap(),
        Counts {
            todo: 2,
            partial: 1,
            ported: 1,
            not_applicable: 1
        }
    );
}

#[test]
fn coverage_includes_build_benchmark_and_fixture_files_outside_src() {
    let mut fixture = Fixture::new();
    for source in [
        "build.zig",
        "benchmarks/sample.zig",
        "tests/fixtures/sample.zig",
    ] {
        let path = fixture.upstream.path().join(source);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }
    fixture.commit_sources();
    fixture.map(&row("src/a.zig", "todo", "", ""));
    let error = fixture.check().unwrap_err();
    for source in [
        "build.zig",
        "benchmarks/sample.zig",
        "tests/fixtures/sample.zig",
    ] {
        assert!(error.contains(source), "missing {source} in {error}");
    }
}

#[test]
fn coverage_ignores_untracked_and_dirty_checkout_contents() {
    let fixture = Fixture::new();
    fixture.map(&row("src/a.zig", "todo", "", ""));
    fs::write(fixture.upstream.path().join("src/untracked.zig"), "").unwrap();
    fs::remove_file(fixture.upstream.path().join("src/a.zig")).unwrap();
    assert_eq!(
        fixture.check().unwrap(),
        Counts {
            todo: 1,
            ..Counts::default()
        }
    );
}

#[test]
fn wrong_checkout_is_reported_before_coverage_errors() {
    let fixture = Fixture::new();
    fixture.map("");
    assert!(
        fixture_git(
            fixture.upstream.path(),
            &["commit", "--allow-empty", "-qm", "test: different tree"]
        )
        .status
        .success()
    );
    let error = fixture.check().unwrap_err();
    assert!(
        error.contains("HEAD") && error.contains(&fixture.pin),
        "{error}"
    );
    assert!(!error.contains("missing upstream"), "{error}");
}

#[test]
fn invalid_pins_and_documentation_sync_points_are_rejected() {
    let fixture = Fixture::new();
    fixture.map(&row("src/a.zig", "todo", "", ""));
    for pin in ["short".to_owned(), "A".repeat(40)] {
        fs::write(fixture.root.path().join("parity/UPSTREAM"), pin).unwrap();
        assert!(fixture.check().unwrap_err().contains("UPSTREAM"));
    }
    fs::write(fixture.root.path().join("parity/UPSTREAM"), &fixture.pin).unwrap();
    fs::write(
        fixture.root.path().join("docs/upstream-parity.md"),
        "- **Sync point:** `0000000`\n",
    )
    .unwrap();
    assert!(fixture.check().unwrap_err().contains("Sync point"));
}

#[test]
fn reports_all_schema_module_and_coverage_errors_across_files() {
    let fixture = Fixture::new();
    let rows = row("src/a.zig", "partial", "", "")
        + &row("src/a.zig", "ported", "'crates/missing.rs'", "")
        + &row("src/stale.zig", "not-applicable", "", "")
        + &row("src/unknown.zig", "unknown", "", "");
    fixture.map(&rows);
    fs::write(
        fixture.root.path().join("parity/files/second.toml"),
        row("src/second.zig", "ported", "'../outside.rs'", ""),
    )
    .unwrap();
    let error = fixture.check().unwrap_err();
    for finding in [
        "requires modules",
        "requires a concrete note",
        "duplicate",
        "crates/missing.rs",
        "stale",
        "unknown",
        "../outside.rs",
    ] {
        assert!(error.contains(finding), "missing {finding}: {error}");
    }
}

#[test]
fn errors_in_one_map_do_not_hide_errors_in_other_maps() {
    let fixture = Fixture::new();
    fixture.map("invalid = [");
    fs::write(
        fixture.root.path().join("parity/files/second.toml"),
        row("src/a.zig", "partial", "", ""),
    )
    .unwrap();
    let error = fixture.check().unwrap_err();
    assert!(
        error.contains("example.toml") && error.contains("requires modules"),
        "{error}"
    );
}

#[test]
fn invalid_paths_and_unknown_fields_are_rejected() {
    let fixture = Fixture::new();
    for module in [
        "crates/missing.rs",
        "../outside.rs",
        "/absolute.rs",
        "crates/example/src/lib.txt",
        "crates\\example.rs",
    ] {
        fixture.map(&row("src/a.zig", "ported", &format!("'{module}'"), ""));
        assert!(fixture.check().unwrap_err().contains("module"));
    }
    fixture.map(&(row("src/a.zig", "todo", "", "") + "moduls = []\n"));
    assert!(fixture.check().unwrap_err().contains("moduls"));
}

#[cfg(unix)]
#[test]
fn module_symlink_escape_is_rejected() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("outside.rs"), "").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("outside.rs"),
        fixture.root.path().join("escape.rs"),
    )
    .unwrap();
    fixture.map(&row("src/a.zig", "ported", "'escape.rs'", ""));
    assert!(fixture.check().unwrap_err().contains("module"));
}

#[test]
fn upstream_option_overrides_environment_and_rejects_unknown_arguments() {
    assert_eq!(
        upstream_path(&["--upstream", "chosen"], Some("fallback".into())).unwrap(),
        PathBuf::from("chosen")
    );
    assert_eq!(
        upstream_path(&[], Some("fallback".into())).unwrap(),
        PathBuf::from("fallback")
    );
    assert!(
        upstream_path(&[], None)
            .unwrap_err()
            .contains("OH_FX_UPSTREAM")
    );
    for args in [
        vec!["--fetch"],
        vec!["--upstream"],
        vec!["--upstream", "a", "--upstream", "b"],
        vec!["--upstream", ""],
    ] {
        assert!(upstream_path(&args, None).is_err());
    }
}

#[test]
fn local_validation_catches_renamed_modules_without_an_upstream_checkout() {
    let fixture = Fixture::new();
    fixture.map(&row(
        "src/a.zig",
        "ported",
        "'crates/example/src/lib.rs'",
        "",
    ));
    fs::remove_dir_all(fixture.upstream.path()).unwrap();
    assert!(check_local(fixture.root.path()).is_ok());
    fs::rename(
        fixture.root.path().join("crates/example/src/lib.rs"),
        fixture.root.path().join("crates/example/src/renamed.rs"),
    )
    .unwrap();
    assert!(
        check_local(fixture.root.path())
            .unwrap_err()
            .contains("crates/example/src/lib.rs")
    );
}

#[test]
fn empty_pinned_git_trees_are_rejected() {
    let mut fixture = Fixture::new();
    fs::remove_file(fixture.upstream.path().join("src/a.zig")).unwrap();
    fixture.commit_sources();
    fixture.map("");
    assert!(fixture.check().unwrap_err().contains("empty source tree"));
}

#[test]
fn git_commands_ignore_repository_and_config_environment_overrides() {
    const CHILD: &str = "OH_FX_PARITY_GIT_ENV_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "parity::tests::git_commands_ignore_repository_and_config_environment_overrides",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("GIT_DIR", "/not/a/repository")
            .env("GIT_WORK_TREE", "/not/a/worktree")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
            .env("GIT_CONFIG_VALUE_0", "true")
            .env("GIT_INDEX_FILE", "/not/an/index")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let fixture = Fixture::new();
    fixture.map(&row("src/a.zig", "todo", "", ""));
    assert!(fixture.check().is_ok());
    let identity = String::from_utf8(
        fixture_git(
            fixture.upstream.path(),
            &["log", "-1", "--format=%an <%ae>"],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(identity.trim(), "Parity Fixture <parity@example.invalid>");
}

#[test]
fn local_validation_collects_invalid_pin_and_module_errors_together() {
    let fixture = Fixture::new();
    fixture.map(&row("src/a.zig", "partial", "'missing.rs'", ""));
    fs::write(fixture.root.path().join("parity/UPSTREAM"), "short").unwrap();
    let error = check_local(fixture.root.path()).unwrap_err();
    for finding in ["UPSTREAM", "missing.rs", "requires a concrete note"] {
        assert!(error.contains(finding), "missing {finding}: {error}");
    }
}
