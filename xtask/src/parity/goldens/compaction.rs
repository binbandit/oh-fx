pub(super) fn extract(source: &str) -> Result<String, String> {
    const DECLARATION: &str = "pub const system_prompt =";
    if source.lines().filter(|line| *line == DECLARATION).count() != 1 {
        return Err("expected one compaction system_prompt declaration".to_owned());
    }
    let (_, body) = source.split_once(DECLARATION).ok_or("missing prompt")?;
    let mut output = String::new();
    for line in body.lines().skip(1) {
        let line = line.trim();
        let (literal, finished) = if let Some(literal) = line.strip_suffix(';') {
            (literal, true)
        } else if let Some(literal) = line.strip_suffix(" ++") {
            (literal, false)
        } else {
            return Err("unsupported compaction prompt declaration".to_owned());
        };
        let literal = literal
            .strip_prefix('"')
            .and_then(|text| text.strip_suffix('"'))
            .ok_or("unsupported compaction prompt literal")?;
        if literal.contains(['"', '\\']) {
            return Err("unsupported compaction prompt escape".to_owned());
        }
        output.push_str(literal);
        if finished {
            return if output.is_empty() {
                Err("empty compaction prompt".to_owned())
            } else {
                Ok(output)
            };
        }
    }
    Err("unterminated compaction prompt declaration".to_owned())
}
