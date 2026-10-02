use ofx_text::{
    encode_terminal_safe, trim_break_whitespace, visible_width, wrap_cut_ignoring_ansi,
};

pub(crate) fn project_command_text(command: &str) -> String {
    let mut projected = String::with_capacity(command.len());
    for (index, line) in command.split('\n').enumerate() {
        if index > 0 {
            projected.push('\n');
        }
        projected.push_str(&encode_terminal_safe(line.as_bytes(), usize::MAX).text);
    }
    projected
}

pub(crate) fn command_segments(encoded: &str, content_width: usize) -> Option<Vec<&str>> {
    if encoded.is_empty() {
        return Some(vec![""]);
    }
    let mut segments = CommandSegments {
        remaining: encoded,
        content_width,
        trailing_empty_line: false,
    };
    let mut collected = Vec::new();
    while let Some(segment) = segments.next_segment().ok()? {
        collected.push(segment);
    }
    Some(collected)
}

struct TooNarrow;

struct CommandSegments<'a> {
    remaining: &'a str,
    content_width: usize,
    trailing_empty_line: bool,
}

impl<'a> CommandSegments<'a> {
    fn next_segment(&mut self) -> Result<Option<&'a str>, TooNarrow> {
        if self.remaining.is_empty() {
            if !self.trailing_empty_line {
                return Ok(None);
            }
            self.trailing_empty_line = false;
            return Ok(Some(""));
        }
        if self.content_width == 0 {
            return Err(TooNarrow);
        }
        let line_end = self.remaining.find('\n').unwrap_or(self.remaining.len());
        let line = &self.remaining[..line_end];
        if line.is_empty() {
            self.consume_hard_line(line_end);
            return Ok(Some(""));
        }
        let hard_cut = prefix_terminal_safe_by_width(line, self.content_width);
        if hard_cut.is_empty() {
            return Err(TooNarrow);
        }
        let segment = if hard_cut.len() < line.len() {
            let soft_cut = wrap_cut_ignoring_ansi(line, self.content_width);
            if !soft_cut.is_empty() && soft_cut.len() < hard_cut.len() {
                soft_cut
            } else {
                hard_cut
            }
        } else {
            hard_cut
        };
        if segment.len() == line.len() {
            self.consume_hard_line(line_end);
        } else {
            let untrimmed = &line[segment.len()..];
            let trimmed = trim_break_whitespace(untrimmed);
            self.remaining = &self.remaining[segment.len() + untrimmed.len() - trimmed.len()..];
        }
        Ok(Some(segment))
    }

    fn consume_hard_line(&mut self, line_end: usize) {
        if line_end == self.remaining.len() {
            self.remaining = "";
            return;
        }
        self.remaining = &self.remaining[line_end + 1..];
        self.trailing_empty_line = self.remaining.is_empty();
    }
}

pub(crate) fn prefix_terminal_safe_by_width(encoded: &str, max_width: usize) -> &str {
    let mut width = 0;
    let mut end = 0;
    while end < encoded.len() {
        let token_end = end + encoded_token_len(&encoded[end..]);
        let token_width = visible_width(&encoded[end..token_end]);
        if width + token_width > max_width {
            break;
        }
        width += token_width;
        end = token_end;
    }
    &encoded[..end]
}

pub(crate) fn suffix_terminal_safe_by_width(encoded: &str, max_width: usize) -> &str {
    let mut width = visible_width(encoded);
    let mut start = 0;
    while start < encoded.len() && width > max_width {
        let token_end = start + encoded_token_len(&encoded[start..]);
        width = width.saturating_sub(visible_width(&encoded[start..token_end]));
        start = token_end;
    }
    &encoded[start..]
}

fn encoded_token_len(encoded: &str) -> usize {
    let bytes = encoded.as_bytes();
    if bytes.len() >= 4
        && bytes[0] == b'\\'
        && bytes[1] == b'x'
        && bytes[2].is_ascii_hexdigit()
        && bytes[3].is_ascii_hexdigit()
    {
        return 4;
    }
    if bytes.len() >= 5 && bytes.starts_with(b"\\u{") {
        let mut saw_hex = false;
        for (index, byte) in bytes.iter().enumerate().take(12).skip(3) {
            if *byte == b'}' {
                if saw_hex {
                    return index + 1;
                }
                break;
            }
            if !byte.is_ascii_hexdigit() {
                break;
            }
            saw_hex = true;
        }
    }
    encoded.chars().next().map_or(1, char::len_utf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_command_projection_keeps_literal_escapes_and_encodes_controls() {
        assert_eq!(
            project_command_text("printf '\\x0a'\nprintf '\x1b[31m'\r"),
            "printf '\\x0a'\nprintf '\\x1b[31m'\\x0d"
        );
    }

    #[test]
    fn command_segments_retain_empty_hard_lines() {
        assert_eq!(
            command_segments("one\n\nthree\n", 80).unwrap(),
            ["one", "", "three", ""]
        );
    }

    #[test]
    fn inline_command_panel_wraps_commands_at_word_boundaries() {
        let width = 28 - visible_width("  $ ");
        assert_eq!(
            command_segments("curl --header alpha --header bravo", width).unwrap(),
            ["curl --header alpha", "--header bravo"]
        );
    }

    #[test]
    fn inline_command_panel_preserves_hard_newlines_from_the_raw_command() {
        let encoded = project_command_text("cat <<'EOF'\nline one\nEOF");
        assert_eq!(
            command_segments(&encoded, 116).unwrap(),
            ["cat <<'EOF'", "line one", "EOF"]
        );
        assert!(!encoded.contains("\\x0a"));
    }

    #[test]
    fn inline_command_rows_account_for_terminal_safe_escape_width() {
        let encoded = project_command_text(&format!("{}\n", "x".repeat(75)));
        assert_eq!(
            command_segments(&encoded, 80 - visible_width("  $ ")).unwrap(),
            [&"x".repeat(75), ""]
        );
    }

    #[test]
    fn encoded_controls_wrap_at_their_visible_width() {
        let encoded = project_command_text(&format!("{}\x07", "x".repeat(72)));
        assert_eq!(command_segments(&encoded, 76).unwrap().len(), 1);
        let encoded = project_command_text(&format!("{}\x07", "x".repeat(73)));
        assert_eq!(
            command_segments(&encoded, 76).unwrap(),
            [&"x".repeat(73), "\\x07"]
        );
    }

    #[test]
    fn segments_never_split_an_escape_and_report_commands_that_cannot_fit() {
        assert_eq!(prefix_terminal_safe_by_width("ab\\x1bcd", 4), "ab");
        assert_eq!(suffix_terminal_safe_by_width("ab\\x1bcd", 3), "cd");
        assert_eq!(
            suffix_terminal_safe_by_width("a\\u{202e}b", 9),
            "\\u{202e}b"
        );
        assert_eq!(command_segments("abc", 0), None);
        assert_eq!(command_segments("\\x1b", 3), None);
        assert_eq!(command_segments("", 0).unwrap(), [""]);
    }
}
