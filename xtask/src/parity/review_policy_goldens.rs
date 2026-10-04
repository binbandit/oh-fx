use std::path::Path;

pub(super) fn run(options: &[&str]) -> Result<(), String> {
    let upstream =
        super::upstream_path(options, std::env::var_os("OH_FX_UPSTREAM").map(Into::into))?;
    let upstream = upstream.canonicalize().map_err(|error| error.to_string())?;
    crate::workspace_files::enter_repository_root()?;
    regenerate(Path::new("."), &upstream)
}

fn regenerate(root: &Path, upstream: &Path) -> Result<(), String> {
    let pin = super::read_pin(root)?;
    super::git(
        upstream,
        &[
            "--no-lazy-fetch",
            "rev-parse",
            "--verify",
            &format!("{pin}^{{commit}}"),
        ],
    )?;
    let source = super::git(
        upstream,
        &[
            "--no-lazy-fetch",
            "show",
            &format!("{pin}:src/core/permissions/auto_classifier.zig"),
        ],
    )?;
    let policy = extract(&source)?;
    let destination = root.join("parity/goldens/review-policy");
    std::fs::create_dir_all(&destination).map_err(|error| error.to_string())?;
    std::fs::write(destination.join("review_policy.xml"), policy).map_err(|error| error.to_string())
}

fn extract(source: &str) -> Result<String, String> {
    const DECLARATION: &str = "const review_policy_template =";
    if source.lines().filter(|line| *line == DECLARATION).count() != 1 {
        return Err("expected one review_policy_template declaration".to_owned());
    }
    let (_, body) = source
        .split_once(DECLARATION)
        .ok_or("missing review policy")?;
    let mut lines = Vec::new();
    for line in body.lines().skip(1) {
        if line == ";" {
            let policy = lines.join("\n");
            if policy.is_empty() || !policy.ends_with('\n') {
                return Err(
                    "review policy must be nonempty and retain its final newline".to_owned(),
                );
            }
            return Ok(policy);
        }
        let content = line
            .strip_prefix(r"    \\")
            .ok_or("unsupported review policy multiline syntax")?;
        lines.push(content);
    }
    Err("unterminated review policy declaration".to_owned())
}

#[cfg(test)]
mod tests;
