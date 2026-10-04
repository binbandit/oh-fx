use std::fmt::Write as _;

pub(crate) fn write_compact(out: &mut String, json: &str) {
    let mut in_string = false;
    let mut escape_pending = false;
    for character in json.chars() {
        if !in_string {
            if !matches!(character, ' ' | '\t' | '\n' | '\r') {
                in_string = character == '"';
                out.push(character);
            }
            continue;
        }
        if character < '\u{20}' {
            let _ = write!(out, "\\u{:04x}", u32::from(character));
            escape_pending = false;
            continue;
        }
        out.push(character);
        if escape_pending {
            escape_pending = false;
        } else if character == '\\' {
            escape_pending = true;
        } else if character == '"' {
            in_string = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compact(json: &str) -> String {
        let mut out = String::new();
        write_compact(&mut out, json);
        out
    }

    #[test]
    fn compact_json_escapes_raw_control_bytes_inside_strings() {
        assert_eq!(compact("{\"k\": \"a\nb\"}"), "{\"k\":\"a\\u000ab\"}");
    }

    #[test]
    fn compact_json_escapes_a_raw_control_byte_after_a_backslash() {
        let out = compact("{\"k\":\"a\\\nb\"}");
        assert!(!out.contains('\n'));
        assert_eq!(out, "{\"k\":\"a\\\\u000ab\"}");
    }

    #[test]
    fn compact_json_preserves_escaped_quotes_and_backslashes() {
        assert_eq!(
            compact("{\"k\": \"say \\\"hi now\\\"\", \"p\": \"x\\\\\", \"q\": 1}"),
            "{\"k\":\"say \\\"hi now\\\"\",\"p\":\"x\\\\\",\"q\":1}"
        );
    }
}
