const OPERATORS: &[u8] = b"+#./;?&";
const INVALID: &str = "McpResourceTemplateInvalid";
const MISSING: &str = "McpResourceTemplateArgumentMissing";

pub(super) fn template_fields(uri_template: &str) -> Result<Vec<String>, &'static str> {
    let mut fields: Vec<String> = Vec::new();
    for expression in expressions(uri_template)? {
        for variable in expression.split(',') {
            let name = variable_name(without_operator(variable));
            if !name.is_empty() && !fields.iter().any(|field| field == name) {
                fields.push(name.to_owned());
            }
        }
    }
    Ok(fields)
}

pub(super) fn expand_template<'a>(
    uri_template: &str,
    value: impl Fn(&str) -> Option<&'a str>,
) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut remaining = uri_template;
    while let Some(open) = remaining.find('{') {
        out.push_str(&remaining[..open]);
        let after_open = &remaining[open + 1..];
        let close = after_open.find('}').ok_or(INVALID)?;
        let expression = &after_open[..close];
        let operator = *expression.as_bytes().first().ok_or(INVALID)?;
        let variables = without_operator(expression);
        let (prefix, separator) = match operator {
            b'#' => ("#", ","),
            b'.' => (".", "."),
            b'/' => ("/", "/"),
            b';' => (";", ";"),
            b'?' => ("?", "&"),
            b'&' => ("&", "&"),
            _ => ("", ","),
        };
        out.push_str(prefix);
        for (index, variable) in variables.split(',').enumerate() {
            let name = variable_name(variable);
            let value = value(name).ok_or(MISSING)?;
            if index > 0 {
                out.push_str(separator);
            }
            if matches!(operator, b';' | b'?' | b'&') {
                encode(&mut out, name, false);
                out.push('=');
            }
            encode(&mut out, value, matches!(operator, b'+' | b'#'));
        }
        remaining = &after_open[close + 1..];
    }
    out.push_str(remaining);
    Ok(out)
}

fn expressions(uri_template: &str) -> Result<Vec<&str>, &'static str> {
    let mut found = Vec::new();
    let mut remaining = uri_template;
    while let Some(open) = remaining.find('{') {
        let after_open = &remaining[open + 1..];
        let close = after_open.find('}').ok_or(INVALID)?;
        found.push(&after_open[..close]);
        remaining = &after_open[close + 1..];
    }
    Ok(found)
}

fn without_operator(text: &str) -> &str {
    match text.as_bytes().first() {
        Some(byte) if OPERATORS.contains(byte) => &text[1..],
        _ => text,
    }
}

fn variable_name(variable: &str) -> &str {
    variable
        .find([':', '*'])
        .map_or(variable, |modifier| &variable[..modifier])
}

fn encode(out: &mut String, value: &str, allow_reserved: bool) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || b"-._~".contains(&byte);
        let reserved = allow_reserved && b":/?#[]@!$&'()*+,;=".contains(&byte);
        if unreserved || reserved {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
}

#[cfg(test)]
mod tests;
