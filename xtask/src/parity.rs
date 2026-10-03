use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Map {
    #[serde(default)]
    file: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    upstream: String,
    status: String,
    modules: Vec<String>,
    note: Option<String>,
}

pub(crate) fn run(options: &[&str]) -> Result<(), String> {
    let upstream = upstream_path(
        options,
        std::env::var_os("OH_FX_UPSTREAM").map(PathBuf::from),
    )?;
    let upstream = upstream
        .canonicalize()
        .map_err(|error| format!("upstream {}: {error}", upstream.display()))?;
    crate::workspace_files::enter_repository_root()?;
    let counts = check(Path::new("."), &upstream)?;
    let pin = crate::workspace_files::read(Path::new("parity/UPSTREAM"))?;
    verify_checkout(&upstream, pin.trim())?;
    for (status, count) in ["todo", "partial", "ported", "not-applicable"]
        .iter()
        .zip(counts)
    {
        println!("{status}: {count}");
    }
    Ok(())
}

fn check(root: &Path, upstream: &Path) -> Result<[usize; 4], String> {
    let pin = crate::workspace_files::read(&root.join("parity/UPSTREAM"))?;
    let pin = pin.trim();
    if pin.len() != 40
        || !pin
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(
            "parity/UPSTREAM must contain one full 40-character hexadecimal commit".to_owned(),
        );
    }
    let sources = sources(upstream)?;
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let entries = entries(&root)?;
    let mut counts = [0; 4];
    let mut seen = BTreeSet::new();
    for (origin, entry) in entries {
        if !seen.insert(entry.upstream.clone()) {
            return Err(format!(
                "duplicate upstream entry: {} ({origin})",
                entry.upstream
            ));
        }
        if !sources.contains(&entry.upstream) {
            return Err(format!(
                "stale upstream entry: {} ({origin})",
                entry.upstream
            ));
        }
        let status = status_index(&entry.status)?;
        if matches!(status, 1 | 2) && entry.modules.is_empty() {
            return Err(format!("{} requires modules", entry.upstream));
        }
        if matches!(status, 1 | 3)
            && entry
                .note
                .as_deref()
                .is_none_or(|note| note.trim().is_empty())
        {
            return Err(format!("{} requires a concrete note", entry.upstream));
        }
        for module in &entry.modules {
            validate_module(&root, module)?;
        }
        counts[status] += 1;
    }
    let missing: Vec<_> = sources.difference(&seen).cloned().collect();
    if !missing.is_empty() {
        return Err(format!("missing upstream entries:\n{}", missing.join("\n")));
    }
    Ok(counts)
}

fn status_index(status: &str) -> Result<usize, String> {
    match status {
        "todo" => Ok(0),
        "partial" => Ok(1),
        "ported" => Ok(2),
        "not-applicable" => Ok(3),
        _ => Err(format!("invalid parity status: {status}")),
    }
}

fn validate_module(root: &Path, module: &str) -> Result<(), String> {
    let path = Path::new(module);
    if module.is_empty()
        || module.contains('\\')
        || path.extension().is_none_or(|extension| extension != "rs")
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!("invalid module path: {module}"));
    }
    let resolved = root
        .join(path)
        .canonicalize()
        .map_err(|error| format!("module {module}: {error}"))?;
    if !resolved.starts_with(root) || !resolved.is_file() {
        return Err(format!("module is not a repository Rust file: {module}"));
    }
    Ok(())
}

fn entries(root: &Path) -> Result<Vec<(String, Entry)>, String> {
    let directory = root.join("parity/files");
    let mut maps = BTreeSet::new();
    for item in
        fs::read_dir(&directory).map_err(|error| format!("{}: {error}", directory.display()))?
    {
        let item = item.map_err(|error| error.to_string())?;
        if item
            .file_type()
            .map_err(|error| error.to_string())?
            .is_file()
            && item
                .path()
                .extension()
                .is_some_and(|extension| extension == "toml")
        {
            maps.insert(item.path());
        }
    }
    let mut entries = Vec::new();
    for path in maps {
        let text = crate::workspace_files::read(&path)?;
        let map: Map =
            toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        entries.extend(
            map.file
                .into_iter()
                .map(|entry| (path.display().to_string(), entry)),
        );
    }
    Ok(entries)
}

fn sources(upstream: &Path) -> Result<BTreeSet<String>, String> {
    let mut pending = vec![upstream.join("src")];
    let mut sources = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        if fs::symlink_metadata(&directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?
            .file_type()
            .is_symlink()
        {
            return Err(format!(
                "upstream source directory is a symlink: {}",
                directory.display()
            ));
        }
        for item in
            fs::read_dir(&directory).map_err(|error| format!("{}: {error}", directory.display()))?
        {
            let item = item.map_err(|error| error.to_string())?;
            let kind = item.file_type().map_err(|error| error.to_string())?;
            if kind.is_dir() {
                pending.push(item.path());
            } else if kind.is_file()
                && item
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "zig")
            {
                let path = item.path();
                let relative = path
                    .strip_prefix(upstream)
                    .map_err(|error| error.to_string())?;
                let relative = relative
                    .to_str()
                    .ok_or_else(|| format!("non-UTF-8 upstream source: {}", path.display()))?;
                sources.insert(relative.replace('\\', "/"));
            }
        }
    }
    if sources.is_empty() {
        return Err("upstream src contains no Zig files (empty source tree)".to_owned());
    }
    Ok(sources)
}

fn verify_checkout(upstream: &Path, pin: &str) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .current_dir(upstream)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| format!("upstream HEAD: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot read upstream HEAD: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let head = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    if head.trim() != pin {
        return Err(format!(
            "upstream HEAD {} does not match parity/UPSTREAM {pin}",
            head.trim()
        ));
    }
    Ok(())
}

fn upstream_path(options: &[&str], fallback: Option<PathBuf>) -> Result<PathBuf, String> {
    match options {
        ["--upstream", path] if !path.is_empty() => Ok(PathBuf::from(path)),
        [] => fallback.ok_or_else(|| "provide --upstream PATH or OH_FX_UPSTREAM".to_owned()),
        _ => Err("usage: cargo xtask parity [--upstream PATH]".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        root: tempfile::TempDir,
        upstream: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let fixture = Self {
                root: tempfile::tempdir().unwrap(),
                upstream: tempfile::tempdir().unwrap(),
            };
            fs::create_dir_all(fixture.root.path().join("parity/files")).unwrap();
            fs::write(fixture.root.path().join("parity/UPSTREAM"), "a".repeat(40)).unwrap();
            fs::create_dir_all(fixture.upstream.path().join("src")).unwrap();
            fs::write(fixture.upstream.path().join("src/a.zig"), "").unwrap();
            fs::create_dir_all(fixture.root.path().join("crates/example/src")).unwrap();
            fs::write(fixture.root.path().join("crates/example/src/lib.rs"), "").unwrap();
            fixture
        }

        fn map(&self, rows: &str) {
            fs::write(self.root.path().join("parity/files/example.toml"), rows).unwrap();
        }

        fn check(&self) -> Result<[usize; 4], String> {
            check(self.root.path(), self.upstream.path())
        }
    }

    fn row(status: &str, modules: &str, note: &str) -> String {
        format!(
            "[[file]]\nupstream = 'src/a.zig'\nstatus = '{status}'\nmodules = [{modules}]\n{note}"
        )
    }

    #[test]
    fn complete_map_counts_each_status() {
        let fixture = Fixture::new();
        let mut rows = String::new();
        for (index, status) in ["todo", "partial", "ported", "not-applicable"]
            .iter()
            .enumerate()
        {
            let source = format!("src/{index}.zig");
            fs::write(fixture.upstream.path().join(&source), "").unwrap();
            rows.push_str(
                &row(
                    status,
                    "'crates/example/src/lib.rs'",
                    "note = 'concrete reason'\n",
                )
                .replace("src/a.zig", &source),
            );
        }
        rows.push_str(&row("todo", "", ""));
        fixture.map(&rows);
        assert_eq!(fixture.check().unwrap(), [2, 1, 1, 1]);
    }

    #[test]
    fn rejects_missing_duplicate_and_stale_sources() {
        let fixture = Fixture::new();
        fixture.map("");
        assert!(fixture.check().unwrap_err().contains("src/a.zig"));
        let valid = row("todo", "", "");
        fixture.map(&format!("{valid}\n{valid}"));
        assert!(fixture.check().unwrap_err().contains("duplicate"));
        fixture.map(&valid.replace("src/a.zig", "src/stale.zig"));
        assert!(fixture.check().unwrap_err().contains("stale"));
    }

    #[test]
    fn rejects_invalid_status_required_modules_notes_and_module_paths() {
        let fixture = Fixture::new();
        for (status, modules, note, message) in [
            ("unknown", "", "", "status"),
            ("partial", "", "note = 'reason'", "modules"),
            ("ported", "", "", "modules"),
            ("partial", "'crates/example/src/lib.rs'", "", "note"),
            ("not-applicable", "", "note = '   '", "note"),
            ("ported", "'crates/missing.rs'", "", "module"),
            ("ported", "'../outside.rs'", "", "module"),
            ("ported", "'/absolute.rs'", "", "module"),
            ("ported", "'crates/example/src/lib.txt'", "", "module"),
        ] {
            fixture.map(&row(status, modules, note));
            assert!(
                fixture.check().unwrap_err().contains(message),
                "{status} {modules}"
            );
        }
    }

    #[test]
    fn rejects_invalid_pin_and_missing_upstream_source_tree() {
        let fixture = Fixture::new();
        fixture.map(&row("todo", "", ""));
        fs::write(fixture.root.path().join("parity/UPSTREAM"), "short").unwrap();
        assert!(fixture.check().unwrap_err().contains("UPSTREAM"));
        fs::write(fixture.root.path().join("parity/UPSTREAM"), "a".repeat(40)).unwrap();
        fs::remove_dir_all(fixture.upstream.path().join("src")).unwrap();
        assert!(fixture.check().is_err());
    }

    #[test]
    fn rejects_uppercase_pin_empty_sources_and_duplicate_rows_across_files() {
        let fixture = Fixture::new();
        fixture.map(&row("todo", "", ""));
        fs::write(fixture.root.path().join("parity/UPSTREAM"), "A".repeat(40)).unwrap();
        assert!(fixture.check().is_err());
        fs::write(fixture.root.path().join("parity/UPSTREAM"), "a".repeat(40)).unwrap();
        fs::remove_file(fixture.upstream.path().join("src/a.zig")).unwrap();
        assert!(fixture.check().unwrap_err().contains("empty"));
        fs::write(fixture.upstream.path().join("src/a.zig"), "").unwrap();
        fs::write(
            fixture.root.path().join("parity/files/second.toml"),
            row("todo", "", ""),
        )
        .unwrap();
        assert!(fixture.check().unwrap_err().contains("duplicate"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_module_symlink_escape_and_does_not_follow_upstream_symlinks() {
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("outside.rs"), "").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside.rs"),
            fixture.root.path().join("escape.rs"),
        )
        .unwrap();
        fixture.map(&row("ported", "'escape.rs'", ""));
        assert!(fixture.check().unwrap_err().contains("module"));
        fixture.map(&row("todo", "", ""));
        fs::write(outside.path().join("extra.zig"), "").unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.upstream.path().join("src/link"))
            .unwrap();
        assert_eq!(fixture.check().unwrap(), [1, 0, 0, 0]);
    }

    #[test]
    fn rejects_unknown_map_fields() {
        let fixture = Fixture::new();
        fixture.map(&(row("todo", "", "") + "\nmoduls = []"));
        assert!(fixture.check().is_err());
    }

    #[test]
    fn verifies_upstream_git_checkout_against_pin() {
        let fixture = Fixture::new();
        let command = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(fixture.upstream.path())
                .args(args)
                .output()
                .unwrap()
        };
        assert!(command(&["init", "-q"]).status.success());
        assert!(
            command(&[
                "-c",
                "user.name=BinBandit",
                "-c",
                "user.email=crazywolf132@gmail.com",
                "commit",
                "--allow-empty",
                "-qm",
                "test(parity): fixture"
            ])
            .status
            .success()
        );
        let head = String::from_utf8(command(&["rev-parse", "HEAD"]).stdout).unwrap();
        assert!(verify_checkout(fixture.upstream.path(), head.trim()).is_ok());
        assert!(
            verify_checkout(fixture.upstream.path(), &"0".repeat(40))
                .unwrap_err()
                .contains("HEAD")
        );
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
        assert!(upstream_path(&["--fetch"], None).is_err());
        assert!(upstream_path(&["--upstream"], None).is_err());
        assert!(upstream_path(&["--upstream", "a", "--upstream", "b"], None).is_err());
        assert!(upstream_path(&["--upstream", ""], Some("fallback".into())).is_err());
    }
}
