use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

mod compaction;
mod review_policy;

type Sources = BTreeMap<&'static str, String>;

const GOLDENS: &str = "parity/goldens";
const SYSTEM_SOURCE: &str = "src/builtins/system_prompt.md";
const COMPACTION_SOURCE: &str = "src/core/compactor/summarize.zig";
const POLICY_SOURCE: &str = "src/core/permissions/auto_classifier.zig";

struct Extractor {
    golden: &'static str,
    sources: &'static [&'static str],
    extract: fn(&Sources) -> Result<String, String>,
}

const EXTRACTORS: &[Extractor] = &[
    Extractor {
        golden: "system_prompt.md",
        sources: &[SYSTEM_SOURCE],
        extract: system,
    },
    Extractor {
        golden: "compaction_system_prompt.txt",
        sources: &[COMPACTION_SOURCE],
        extract: compaction,
    },
    Extractor {
        golden: "review_policy.xml",
        sources: &[POLICY_SOURCE],
        extract: review_policy,
    },
];

pub(super) fn run(options: &[&str]) -> Result<(), String> {
    let (check_only, options) = match options {
        ["--check", rest @ ..] => (true, rest),
        rest => (false, rest),
    };
    let upstream =
        super::upstream_path(options, std::env::var_os("OH_FX_UPSTREAM").map(Into::into))?;
    let upstream = upstream
        .canonicalize()
        .map_err(|error| format!("upstream {}: {error}", upstream.display()))?;
    crate::workspace_files::enter_repository_root()?;
    if check_only {
        check(Path::new("."), &upstream)
    } else {
        regenerate(Path::new("."), &upstream)
    }
}

fn git(upstream: &Path, args: &[&str]) -> Result<String, String> {
    super::git_with_transport(upstream, args, true)
}

fn source<'a>(sources: &'a Sources, path: &str) -> Result<&'a str, String> {
    sources
        .get(path)
        .map(String::as_str)
        .ok_or_else(|| format!("missing source {path}"))
}

fn system(sources: &Sources) -> Result<String, String> {
    Ok(source(sources, SYSTEM_SOURCE)?.to_owned())
}

fn compaction(sources: &Sources) -> Result<String, String> {
    compaction::extract(source(sources, COMPACTION_SOURCE)?)
}

fn review_policy(sources: &Sources) -> Result<String, String> {
    review_policy::extract(source(sources, POLICY_SOURCE)?)
}

fn derive(root: &Path, upstream: &Path) -> Result<Vec<(&'static str, String)>, String> {
    let pin = super::read_pin(root)?;
    git(
        upstream,
        &["rev-parse", "--verify", &format!("{pin}^{{commit}}")],
    )?;
    let paths: BTreeSet<&str> = EXTRACTORS
        .iter()
        .flat_map(|extractor| extractor.sources.iter().copied())
        .collect();
    let mut sources = Sources::new();
    for path in paths {
        sources.insert(path, git(upstream, &["show", &format!("{pin}:{path}")])?);
    }
    EXTRACTORS
        .iter()
        .map(|extractor| Ok((extractor.golden, (extractor.extract)(&sources)?)))
        .collect()
}

fn regenerate(root: &Path, upstream: &Path) -> Result<(), String> {
    let goldens = derive(root, upstream)?;
    let directory = root.join(GOLDENS);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    for (name, content) in goldens {
        let path = directory.join(name);
        std::fs::write(&path, content).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}

fn check(root: &Path, upstream: &Path) -> Result<(), String> {
    let goldens = derive(root, upstream)?;
    let mut errors = Vec::new();
    for (name, content) in goldens {
        let path = root.join(GOLDENS).join(name);
        match std::fs::read(&path) {
            Ok(bytes) if bytes == content.as_bytes() => {}
            Ok(_) => errors.push(format!("{} differs from pinned source", path.display())),
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    crate::report("golden validation", &errors)
}

#[cfg(test)]
mod tests;
