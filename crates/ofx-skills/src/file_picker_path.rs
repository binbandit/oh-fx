fn is_separator(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn is_start_boundary(byte: u8) -> bool {
    is_separator(byte) || matches!(byte, b'(' | b'[' | b'{' | b'<' | b'\'' | b'"' | b'`')
}

fn token_end(text: &[u8], start: usize) -> usize {
    let mut index = start;
    let mut quote: Option<u8> = None;
    while index < text.len() {
        let byte = text[index];
        if byte == b'\\' && index + 1 < text.len() {
            index += 2;
            continue;
        }
        match quote {
            Some(active) if byte == active => quote = None,
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if is_separator(byte) => break,
            _ => {}
        }
        index += 1;
    }
    index
}

pub(crate) fn at_path_end(text: &[u8], start: usize) -> Option<usize> {
    if text.get(start) != Some(&b'@') {
        return None;
    }
    if start > 0 && !is_start_boundary(text[start - 1]) {
        return None;
    }
    let quoted = text.get(start + 1) == Some(&b'"');
    let path_start = start + 1 + usize::from(quoted);
    if !quoted {
        if text.get(path_start) == Some(&b'\'') {
            return Some(token_end(text, path_start));
        }
        let mut end = path_start;
        while end < text.len() && !is_separator(text[end]) {
            if text[end] == b'\\' && end + 1 < text.len() {
                end += 1;
            }
            end += 1;
        }
        return Some(end);
    }
    let mut index = path_start;
    while index < text.len() {
        match text[index] {
            b'\n' | b'\r' | b'\t' => break,
            b'\\' => {
                let Some(&next) = text.get(index + 1) else {
                    return Some(text.len());
                };
                if matches!(next, b'\n' | b'\r' | b'\t') {
                    break;
                }
                index += 2;
            }
            b'"' => return Some(token_end(text, index + 1)),
            _ => index += 1,
        }
    }
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_path_spans_cover_bare_quoted_and_incomplete_tokens() {
        assert_eq!(at_path_end(b"@src/main.zig rest", 0), Some(13));
        assert_eq!(at_path_end(b"@\"a b.png\", rest", 0), Some(11));
        assert_eq!(at_path_end(b"@\"a\\\"b.png\" rest", 0), Some(11));
        assert_eq!(at_path_end(b"@\"unterminated", 0), Some(14));
        assert_eq!(at_path_end(b"@\"tab\there", 0), Some(5));
        assert_eq!(at_path_end(b"@'quoted name' rest", 0), Some(14));
        assert_eq!(at_path_end(b"x@y", 1), None);
        assert_eq!(at_path_end(b"(@y)", 1), Some(4));
        assert_eq!(at_path_end(b"plain", 0), None);
    }
}
