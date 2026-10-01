pub(crate) fn left_trim(line: &str) -> &str {
    line.trim_start_matches([' ', '\t'])
}

pub(crate) fn is_space(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

pub(crate) fn is_blank_markdown_line(line: &str) -> bool {
    line.bytes().all(is_space)
}

pub(crate) fn is_ascii_alpha(byte: u8) -> bool {
    byte.is_ascii_alphabetic()
}

pub(crate) fn is_ascii_alpha_numeric(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

pub(crate) fn is_ascii_word_byte(byte: u8) -> bool {
    is_ascii_alpha_numeric(byte) || byte == b'_'
}

pub(crate) fn is_ascii_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

pub(crate) fn is_trailing_url_punctuation(byte: u8) -> bool {
    matches!(byte, b'.' | b',' | b';' | b':' | b'!' | b'?')
}

fn is_escapable_punctuation(byte: u8) -> bool {
    byte.is_ascii_punctuation()
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EscapedPunctuation {
    backslash_run_start: usize,
    backslash_run_end: usize,
}

impl EscapedPunctuation {
    pub(crate) fn at(&mut self, text: &[u8], index: usize) -> bool {
        if index == 0
            || index >= text.len()
            || !is_escapable_punctuation(text[index])
            || text[index - 1] != b'\\'
        {
            return false;
        }
        let backslash = index - 1;
        if !(self.backslash_run_start..self.backslash_run_end).contains(&backslash) {
            self.backslash_run_start = backslash
                - text[..backslash]
                    .iter()
                    .rev()
                    .take_while(|&&byte| byte == b'\\')
                    .count();
            self.backslash_run_end = index
                + text[index..]
                    .iter()
                    .take_while(|&&byte| byte == b'\\')
                    .count();
        }
        (index - self.backslash_run_start) % 2 == 1
    }
}

pub(crate) fn append_escaped_punctuation(out: &mut String, text: &str) {
    let bytes = text.as_bytes();
    let mut escapes = EscapedPunctuation::default();
    let mut copied_until = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() && escapes.at(bytes, index + 1) {
            out.push_str(&text[copied_until..index]);
            copied_until = index + 1;
            index += 2;
            continue;
        }
        index += 1;
    }
    out.push_str(&text[copied_until..]);
}

pub(crate) fn without_terminal_hard_break_marker(line: &str, line_has_lf: bool) -> &str {
    if !line_has_lf || !line.ends_with('\\') {
        return line;
    }
    let slash_count = line.bytes().rev().take_while(|&byte| byte == b'\\').count();
    if slash_count % 2 == 0 {
        return line;
    }
    &line[..line.len() - 1]
}

pub(crate) fn nth_line(buf: &str, n: usize) -> Option<&str> {
    buf.split_terminator('\n').nth(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_punctuation_counts_preceding_backslashes() {
        assert!(EscapedPunctuation::default().at(b"\\*", 1));
        assert!(!EscapedPunctuation::default().at(b"\\\\*", 2));
        assert!(!EscapedPunctuation::default().at(b"\\a", 1));
        let run = b"x\\\\\\*\\\\*";
        let mut escapes = EscapedPunctuation::default();
        let expected = [false, true, false, true, false, true, false, false];
        for (index, &escaped) in expected.iter().enumerate() {
            assert_eq!(escapes.at(run, index + 1), escaped, "{index}");
        }
        let mut out = String::new();
        append_escaped_punctuation(&mut out, "docs \\*literal\\* \\\\ \\a");
        assert_eq!(out, "docs *literal* \\ \\a");
    }

    #[test]
    fn nth_line_splits_on_line_feeds_without_a_trailing_empty_line() {
        assert_eq!(nth_line("a\nb\n", 1), Some("b"));
        assert_eq!(nth_line("a\nb\n", 2), None);
        assert_eq!(nth_line("a\n\nc", 1), Some(""));
        assert_eq!(nth_line("", 0), None);
    }
}
