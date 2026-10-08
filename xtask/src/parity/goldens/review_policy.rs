pub(super) fn extract(source: &str) -> Result<String, String> {
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
