pub(super) fn section(source: &str) -> Result<String, String> {
    Ok([
        constant(source, "const header =")?,
        constant(source, "const empty_entry =")?,
        constant(source, "const footer =")?,
    ]
    .concat())
}

pub(super) fn change_notice(source: &str) -> Result<String, String> {
    Ok([
        constant(source, "    const change_header =")?,
        constant(source, "    const change_footer =")?,
    ]
    .concat())
}

fn constant(source: &str, declaration: &str) -> Result<String, String> {
    let mut starts = source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.starts_with(declaration));
    let (start, first) = starts
        .next()
        .ok_or_else(|| format!("missing {}", declaration.trim()))?;
    if starts.next().is_some() {
        return Err(format!("expected one {}", declaration.trim()));
    }
    let inline = first[declaration.len()..].trim();
    let parts: Vec<&str> = if inline.is_empty() {
        source.lines().skip(start + 1).map(str::trim).collect()
    } else {
        vec![inline]
    };
    let mut output = String::new();
    for part in parts {
        let (literal, finished) = if let Some(literal) = part.strip_suffix(';') {
            (literal, true)
        } else if let Some(literal) = part.strip_suffix(" ++") {
            (literal, false)
        } else {
            return Err(format!("unsupported {} grammar", declaration.trim()));
        };
        output.push_str(&unescape(literal)?);
        if finished {
            return if output.is_empty() {
                Err(format!("empty {}", declaration.trim()))
            } else {
                Ok(output)
            };
        }
    }
    Err(format!("unterminated {}", declaration.trim()))
}

fn unescape(literal: &str) -> Result<String, String> {
    let text = literal
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .ok_or("unsupported string literal")?;
    let text = text.replace("\\n", "\n");
    if text.contains(['"', '\\']) {
        return Err("unsupported string escape".to_owned());
    }
    Ok(text)
}
