use crate::presentation::text_util::{
    EscapedPunctuation, is_blank_markdown_line, is_space, left_trim,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CodeFence {
    pub(crate) marker: u8,
    pub(crate) run: usize,
    pub(crate) indent: usize,
}

fn leading_space_or_tab_count(line: &str) -> usize {
    line.bytes().take_while(|&byte| is_space(byte)).count()
}

pub(crate) fn parse_code_fence(line: &str) -> Option<CodeFence> {
    let bytes = line.as_bytes();
    let indent = leading_space_or_tab_count(line);
    let marker = *bytes.get(indent)?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let run = bytes[indent..]
        .iter()
        .take_while(|&&byte| byte == marker)
        .count();
    (run >= 3).then_some(CodeFence {
        marker,
        run,
        indent,
    })
}

pub(crate) fn code_fence_marker(line: &str) -> Option<u8> {
    parse_code_fence(line).map(|fence| fence.marker)
}

pub(crate) fn closes_code_fence(line: &str, open: CodeFence) -> bool {
    let Some(fence) = parse_code_fence(line) else {
        return false;
    };
    if fence.marker != open.marker || fence.run < open.run {
        return false;
    }
    is_blank_markdown_line(&line[fence.indent + fence.run..])
}

pub(crate) fn code_fence_language(line: &str) -> &str {
    let Some(fence) = parse_code_fence(line) else {
        return "";
    };
    let info = line[fence.indent + fence.run..].trim_matches([' ', '\t']);
    let end = info.find([' ', '\t']).unwrap_or(info.len());
    &info[..end]
}

pub(crate) fn strip_fence_indent(line: &str, indent: usize) -> &str {
    let stripped = line
        .bytes()
        .take(indent)
        .take_while(|&byte| is_space(byte))
        .count();
    &line[stripped..]
}

pub(crate) fn has_indented_code_prefix(line: &str) -> bool {
    line.starts_with('\t') || line.starts_with("    ")
}

pub(crate) fn deindent_code_line(line: &str) -> &str {
    line.strip_prefix('\t')
        .unwrap_or_else(|| line.get(4..).unwrap_or(""))
}

pub(crate) struct ParsedHeader<'a> {
    pub(crate) level: usize,
    pub(crate) content: &'a str,
}

pub(crate) fn parse_header(line: &str) -> Option<ParsedHeader<'_>> {
    let bytes = line.as_bytes();
    let start = bytes
        .iter()
        .take(3)
        .take_while(|&&byte| byte == b' ')
        .count();
    let level = bytes[start..]
        .iter()
        .take(6)
        .take_while(|&&byte| byte == b'#')
        .count();
    if level == 0 {
        return None;
    }
    let marker_end = start + level;
    if bytes.get(marker_end) != Some(&b' ') {
        return None;
    }
    Some(ParsedHeader {
        level,
        content: without_closing_hashes(&line[marker_end + 1..]),
    })
}

fn without_closing_hashes(content: &str) -> &str {
    let trimmed = content.trim_end_matches([' ', '\t']);
    let without_hashes = trimmed.trim_end_matches('#');
    if without_hashes.len() == trimmed.len() {
        return content;
    }
    if without_hashes.is_empty() {
        return "";
    }
    if !without_hashes.ends_with([' ', '\t']) {
        return content;
    }
    without_hashes.trim_end_matches([' ', '\t'])
}

pub(crate) fn parse_setext_underline(line: &str) -> Option<usize> {
    let mut level = None;
    for byte in line.bytes() {
        if is_space(byte) {
            continue;
        }
        let next_level = match byte {
            b'=' => 1,
            b'-' => 2,
            _ => return None,
        };
        match level {
            Some(existing) if existing != next_level => return None,
            _ => level = Some(next_level),
        }
    }
    level
}

pub(crate) fn is_setext_candidate(line: &str) -> bool {
    let Some(&first) = line.as_bytes().first() else {
        return false;
    };
    if first == b' ' || first == b'\t' || first == b':' {
        return false;
    }
    if parse_header(line).is_some() || parse_blockquote(line).is_some() {
        return false;
    }
    if parse_unordered_list(line).is_some() || parse_ordered_list(line).is_some() {
        return false;
    }
    code_fence_marker(line).is_none()
        && !is_pipe_line(line)
        && !is_horizontal_rule(line)
        && parse_setext_underline(line).is_none()
}

pub(crate) fn definition_marker_body(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    if bytes.len() < 2 || bytes[0] != b':' {
        return None;
    }
    let body_start = 1 + leading_space_or_tab_count(&line[1..]);
    if body_start == 1 || body_start == line.len() {
        return None;
    }
    Some(&line[body_start..])
}

pub(crate) struct ParsedFootnoteDefinition<'a> {
    pub(crate) label: &'a str,
    pub(crate) body: &'a str,
}

pub(crate) fn parse_footnote_definition(line: &str) -> Option<ParsedFootnoteDefinition<'_>> {
    let bytes = line.as_bytes();
    if bytes.len() < 6 || bytes[0] != b'[' || bytes[1] != b'^' {
        return None;
    }
    let close = 2 + line[2..].find(']')?;
    if close == 2 || close + 1 >= bytes.len() || bytes[close + 1] != b':' {
        return None;
    }
    let body_start = close + 2 + leading_space_or_tab_count(&line[close + 2..]);
    if body_start == line.len() {
        return None;
    }
    Some(ParsedFootnoteDefinition {
        label: &line[2..close],
        body: &line[body_start..],
    })
}

pub(crate) fn footnote_continuation_body(line: &str) -> Option<&str> {
    line.strip_prefix('\t').or_else(|| line.strip_prefix("  "))
}

pub(crate) struct ParsedBlockquote<'a> {
    pub(crate) indent: usize,
    pub(crate) depth: usize,
    pub(crate) content: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockquotePrefix {
    pub(crate) indent: usize,
    pub(crate) depth: usize,
}

pub(crate) fn parse_blockquote(line: &str) -> Option<ParsedBlockquote<'_>> {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|&&byte| byte == b' ').count();
    if bytes.get(indent) != Some(&b'>') {
        return None;
    }
    let mut index = indent + 1;
    if index == bytes.len() {
        return Some(ParsedBlockquote {
            indent,
            depth: 1,
            content: &line[index..],
        });
    }
    if bytes[index] != b' ' {
        return None;
    }
    index += 1;

    let mut depth = 1;
    while index < bytes.len() && bytes[index] == b'>' {
        let after_marker = index + 1;
        if after_marker < bytes.len() && bytes[after_marker] != b' ' {
            break;
        }
        depth += 1;
        index = after_marker;
        if index == bytes.len() {
            break;
        }
        index += 1;
    }
    Some(ParsedBlockquote {
        indent,
        depth,
        content: &line[index..],
    })
}

pub(crate) fn is_blockquote_paragraph(line: &str) -> bool {
    is_lazy_blockquote_continuation(line)
}

pub(crate) fn is_lazy_blockquote_continuation(line: &str) -> bool {
    if is_blank_markdown_line(line) {
        return false;
    }
    if parse_blockquote(line).is_some() || parse_header(line).is_some() {
        return false;
    }
    if parse_unordered_list(line).is_some() || parse_ordered_list(line).is_some() {
        return false;
    }
    code_fence_marker(left_trim(line)).is_none() && !is_pipe_line(line) && !is_horizontal_rule(line)
}

pub(crate) struct ParsedUnorderedList<'a> {
    pub(crate) indent: &'a str,
    pub(crate) content: &'a str,
}

pub(crate) fn parse_unordered_list(line: &str) -> Option<ParsedUnorderedList<'_>> {
    let indent_len = leading_space_or_tab_count(line);
    let rest = &line[indent_len..];
    let marker_len = if rest.starts_with(['-', '*', '+']) {
        1
    } else if rest.starts_with('•') {
        '•'.len_utf8()
    } else {
        return None;
    };
    let separator = *rest.as_bytes().get(marker_len)?;
    if !is_space(separator) {
        return None;
    }
    Some(ParsedUnorderedList {
        indent: &line[..indent_len],
        content: &rest[marker_len + 1..],
    })
}

pub(crate) struct ParsedOrderedList<'a> {
    pub(crate) indent: &'a str,
    pub(crate) marker: &'a str,
    pub(crate) content: &'a str,
}

pub(crate) fn parse_ordered_list(line: &str) -> Option<ParsedOrderedList<'_>> {
    let bytes = line.as_bytes();
    let indent_end = leading_space_or_tab_count(line);
    let digits = bytes[indent_end..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let index = indent_end + digits;
    if digits == 0 || digits > 9 || index + 1 >= bytes.len() {
        return None;
    }
    if bytes[index] != b'.' && bytes[index] != b')' {
        return None;
    }
    if !is_space(bytes[index + 1]) {
        return None;
    }
    Some(ParsedOrderedList {
        indent: &line[..indent_end],
        marker: &line[indent_end..=index],
        content: &line[index + 2..],
    })
}

#[derive(Clone, Copy)]
pub(crate) struct ParsedTaskListItem<'a> {
    pub(crate) completed: bool,
    pub(crate) has_separator: bool,
    pub(crate) content: &'a str,
}

pub(crate) fn parse_task_list_item(content: &str) -> Option<ParsedTaskListItem<'_>> {
    let bytes = content.as_bytes();
    if bytes.len() < 3 || bytes[0] != b'[' || bytes[2] != b']' {
        return None;
    }
    let completed = match bytes[1] {
        b' ' => false,
        b'x' | b'X' => true,
        _ => return None,
    };
    if bytes.len() == 3 {
        return Some(ParsedTaskListItem {
            completed,
            has_separator: false,
            content: &content[3..],
        });
    }
    if bytes[3] != b' ' {
        return None;
    }
    Some(ParsedTaskListItem {
        completed,
        has_separator: true,
        content: &content[4..],
    })
}

pub(crate) fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = left_trim(line).as_bytes();
    if trimmed.len() < 3 {
        return false;
    }
    let rule_char = trimmed[0];
    if !matches!(rule_char, b'-' | b'*' | b'_') {
        return false;
    }
    let mut count = 0;
    for &byte in trimmed {
        if byte == rule_char {
            count += 1;
        } else if !is_space(byte) {
            return false;
        }
    }
    count >= 3
}

pub(crate) fn is_pipe_line(line: &str) -> bool {
    let trimmed = left_trim(line).as_bytes();
    let mut escapes = EscapedPunctuation::default();
    trimmed
        .iter()
        .enumerate()
        .any(|(index, &byte)| byte == b'|' && !escapes.at(trimmed, index))
}

fn is_separator_line(line: &str) -> bool {
    let trimmed = left_trim(line);
    if trimmed.is_empty() {
        return false;
    }
    let mut seen_dash = false;
    let mut seen_pipe = false;
    for byte in trimmed.bytes() {
        match byte {
            b'|' => seen_pipe = true,
            b':' | b' ' | b'\t' => {}
            b'-' => seen_dash = true,
            _ => return false,
        }
    }
    seen_dash && seen_pipe
}

pub(crate) fn is_valid_table(buf: &str) -> bool {
    let mut line_count = 0;
    let mut saw_separator = false;
    for line in table_lines(buf) {
        if line_count == 1 && is_separator_line(line) {
            saw_separator = true;
        }
        line_count += 1;
    }
    line_count >= 2 && saw_separator
}

pub(crate) fn table_lines(buf: &str) -> std::str::SplitTerminator<'_, char> {
    buf.split_terminator('\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_fence_language_skips_the_whole_marker_run() {
        assert_eq!(code_fence_language("````md"), "md");
        assert_eq!(code_fence_language("```   zig extra"), "zig");
        assert_eq!(code_fence_language("~~~"), "");
    }

    #[test]
    fn closing_fence_needs_matching_marker_length_and_nothing_after_it() {
        let open = CodeFence {
            marker: b'`',
            run: 4,
            indent: 0,
        };
        assert!(closes_code_fence("````", open));
        assert!(closes_code_fence("`````  ", open));
        assert!(!closes_code_fence("```", open));
        assert!(!closes_code_fence("~~~~", open));
        assert!(!closes_code_fence("```` trailing", open));
    }

    #[test]
    fn table_lines_follow_line_feed_boundaries() {
        assert_eq!(table_lines("a\nb\n").collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(table_lines("a\n\nb").collect::<Vec<_>>(), ["a", "", "b"]);
        assert_eq!(table_lines("").count(), 0);
    }
}
