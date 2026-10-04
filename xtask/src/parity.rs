mod goldens;

mod review_policy_goldens;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Map {
    #[serde(default)]
    file: Vec<toml::Value>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Status {
    Todo,
    Partial,
    Ported,
    NotApplicable,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    todo: usize,
    partial: usize,
    ported: usize,
    not_applicable: usize,
}

impl Counts {
    fn add(&mut self, status: Status) {
        match status {
            Status::Todo => self.todo += 1,
            Status::Partial => self.partial += 1,
            Status::Ported => self.ported += 1,
            Status::NotApplicable => self.not_applicable += 1,
        }
    }

    fn print(&self) {
        println!("todo: {}", self.todo);
        println!("partial: {}", self.partial);
        println!("ported: {}", self.ported);
        println!("not-applicable: {}", self.not_applicable);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    upstream: String,
    status: Status,
    modules: Vec<String>,
    note: Option<String>,
}

struct Ledger {
    counts: Counts,
    sources: BTreeSet<String>,
    errors: Vec<String>,
}

pub(crate) fn run(options: &[&str]) -> Result<(), String> {
    if let ["goldens", rest @ ..] = options {
        return goldens::run(rest);
    }
    if let ["review-policy-goldens", rest @ ..] = options {
        return review_policy_goldens::run(rest);
    }
    let upstream = upstream_path(
        options,
        std::env::var_os("OH_FX_UPSTREAM").map(PathBuf::from),
    )?;
    let upstream = upstream
        .canonicalize()
        .map_err(|error| format!("upstream {}: {error}", upstream.display()))?;
    crate::workspace_files::enter_repository_root()?;
    check(Path::new("."), &upstream)?.print();
    Ok(())
}

pub(crate) fn check_local(root: &Path) -> Result<Counts, String> {
    let pin = match read_pin(root) {
        Ok(pin) => pin,
        Err(error) => return invalid_pin(root, error),
    };
    let ledger = local_ledger(root, Some(&pin))?;
    crate::report("parity validation", &ledger.errors)?;
    Ok(ledger.counts)
}

fn check(root: &Path, upstream: &Path) -> Result<Counts, String> {
    let pin = match read_pin(root) {
        Ok(pin) => pin,
        Err(error) => return invalid_pin(root, error),
    };
    verify_checkout(upstream, &pin)?;
    let sources = sources(upstream, &pin)?;
    let mut ledger = local_ledger(root, Some(&pin))?;
    for stale in ledger.sources.difference(&sources) {
        ledger.errors.push(format!("stale upstream entry: {stale}"));
    }
    for missing in sources.difference(&ledger.sources) {
        ledger
            .errors
            .push(format!("missing upstream entry: {missing}"));
    }
    crate::report("parity validation", &ledger.errors)?;
    Ok(ledger.counts)
}

fn invalid_pin(root: &Path, error: String) -> Result<Counts, String> {
    let mut ledger = local_ledger(root, None)?;
    ledger.errors.insert(0, error);
    crate::report("parity validation", &ledger.errors)?;
    Ok(ledger.counts)
}

fn read_pin(root: &Path) -> Result<String, String> {
    let pin = crate::workspace_files::read(&root.join("parity/UPSTREAM"))?;
    let pin = pin.trim();
    if pin.len() != 40 || !lowercase_hex(pin) {
        return Err(
            "parity/UPSTREAM must contain one full 40-character hexadecimal commit".to_owned(),
        );
    }
    Ok(pin.to_owned())
}

fn lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn local_ledger(root: &Path, pin: Option<&str>) -> Result<Ledger, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let mut errors = Vec::new();
    if let Err(error) = verify_sync_point(&root, pin) {
        errors.push(error);
    }
    let entries = entries(&root, &mut errors);
    let mut counts = Counts::default();
    let mut seen = BTreeSet::new();
    for (origin, entry) in entries {
        if !seen.insert(entry.upstream.clone()) {
            errors.push(format!(
                "duplicate upstream entry: {} ({origin})",
                entry.upstream
            ));
        }
        if !relative_file(&entry.upstream, "zig") {
            errors.push(format!(
                "invalid upstream path: {} ({origin})",
                entry.upstream
            ));
        }
        if matches!(entry.status, Status::Partial | Status::Ported) && entry.modules.is_empty() {
            errors.push(format!("{} requires modules ({origin})", entry.upstream));
        }
        if matches!(entry.status, Status::Partial | Status::NotApplicable)
            && entry
                .note
                .as_deref()
                .is_none_or(|note| note.trim().is_empty())
        {
            errors.push(format!(
                "{} requires a concrete note ({origin})",
                entry.upstream
            ));
        }
        for module in &entry.modules {
            if let Err(error) = validate_module(&root, module) {
                errors.push(format!("{error} ({origin})"));
            }
        }
        counts.add(entry.status);
    }
    Ok(Ledger {
        counts,
        sources: seen,
        errors,
    })
}

fn verify_sync_point(root: &Path, pin: Option<&str>) -> Result<(), String> {
    let document = crate::workspace_files::read(&root.join("docs/upstream-parity.md"))?;
    let mut points = document.lines().filter_map(|line| {
        line.strip_prefix("- **Sync point:** `")?
            .split_once('`')
            .map(|(point, _)| point)
    });
    let point = points
        .next()
        .ok_or_else(|| "docs/upstream-parity.md requires one Sync point".to_owned())?;
    if points.next().is_some()
        || point.len() < 7
        || point.len() > 40
        || !lowercase_hex(point)
        || pin.is_some_and(|pin| !pin.starts_with(point))
    {
        return Err(format!(
            "docs/upstream-parity.md Sync point {point} does not match parity/UPSTREAM {}",
            pin.unwrap_or("invalid pin")
        ));
    }
    Ok(())
}

fn relative_file(value: &str, extension: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !value.contains('\\')
        && path.extension().is_some_and(|actual| actual == extension)
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn validate_module(root: &Path, module: &str) -> Result<(), String> {
    if !relative_file(module, "rs") {
        return Err(format!("invalid module path: {module}"));
    }
    let resolved = root
        .join(module)
        .canonicalize()
        .map_err(|error| format!("module {module}: {error}"))?;
    if !resolved.starts_with(root) || !resolved.is_file() {
        return Err(format!("module is not a repository Rust file: {module}"));
    }
    Ok(())
}

fn entries(root: &Path, errors: &mut Vec<String>) -> Vec<(String, Entry)> {
    let directory = root.join("parity/files");
    let items = match fs::read_dir(&directory) {
        Ok(items) => items,
        Err(error) => {
            errors.push(format!("{}: {error}", directory.display()));
            return Vec::new();
        }
    };
    let mut maps = BTreeSet::new();
    for item in items {
        match item {
            Ok(item) => match item.file_type() {
                Ok(kind)
                    if kind.is_file()
                        && item
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "toml") =>
                {
                    maps.insert(item.path());
                }
                Ok(_) => {}
                Err(error) => errors.push(format!("{}: {error}", item.path().display())),
            },
            Err(error) => errors.push(format!("{}: {error}", directory.display())),
        }
    }
    let mut entries = Vec::new();
    for path in maps {
        match crate::workspace_files::read(&path)
            .and_then(|text| toml::from_str::<Map>(&text).map_err(|error| error.to_string()))
        {
            Ok(map) => {
                for (index, row) in map.file.into_iter().enumerate() {
                    let origin = format!("{} file {}", path.display(), index + 1);
                    match row.try_into::<Entry>() {
                        Ok(entry) => entries.push((origin, entry)),
                        Err(error) => errors.push(format!("{origin}: {error}")),
                    }
                }
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    entries
}

fn sources(upstream: &Path, pin: &str) -> Result<BTreeSet<String>, String> {
    let tree = git(upstream, &["ls-tree", "-r", "--name-only", "-z", pin])?;
    let sources: BTreeSet<_> = tree
        .split('\0')
        .filter(|path| {
            Path::new(path)
                .extension()
                .is_some_and(|extension| extension == "zig")
        })
        .map(str::to_owned)
        .collect();
    if sources.is_empty() {
        return Err(
            "pinned upstream Git tree contains no Zig files (empty source tree)".to_owned(),
        );
    }
    Ok(sources)
}

fn verify_checkout(upstream: &Path, pin: &str) -> Result<(), String> {
    let head = git(upstream, &["rev-parse", "--verify", "HEAD"])?;
    if head.trim() != pin {
        return Err(format!(
            "upstream HEAD {} does not match parity/UPSTREAM {pin}",
            head.trim()
        ));
    }
    Ok(())
}

fn git(upstream: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.current_dir(upstream).args(args);
    for (variable, _) in std::env::vars_os() {
        if variable.to_string_lossy().starts_with("GIT_") {
            command.env_remove(variable);
        }
    }
    let output = command
        .output()
        .map_err(|error| format!("upstream git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "upstream git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn upstream_path(options: &[&str], fallback: Option<PathBuf>) -> Result<PathBuf, String> {
    match options {
        ["--upstream", path] if !path.is_empty() => Ok(PathBuf::from(path)),
        [] => fallback.ok_or_else(|| "provide --upstream PATH or OH_FX_UPSTREAM".to_owned()),
        _ => Err("usage: cargo xtask parity [--upstream PATH]".to_owned()),
    }
}

#[cfg(test)]
mod tests;
