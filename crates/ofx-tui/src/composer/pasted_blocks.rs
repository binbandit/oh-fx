use std::borrow::Cow;
use std::fmt::{self, Write};

use super::entity_spans::Span;

pub(crate) const LARGE_PASTE_CHAR_THRESHOLD: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PastedBlock {
    pub(crate) id: usize,
    pub(crate) text: String,
    pub(crate) line_count: usize,
    pub(crate) span: Span,
}

pub(crate) fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let lines = 1 + text.bytes().filter(|byte| *byte == b'\n').count();
    if text.ends_with('\n') && lines > 1 {
        lines - 1
    } else {
        lines
    }
}

pub(crate) fn should_use_placeholder(text: &str) -> bool {
    text.chars().count() > LARGE_PASTE_CHAR_THRESHOLD
}

pub(crate) fn format_placeholder(id: usize, line_count: usize) -> String {
    let mut placeholder = String::new();
    write_placeholder(&mut placeholder, id, line_count).unwrap_or_default();
    placeholder
}

fn write_placeholder(out: &mut impl Write, id: usize, line_count: usize) -> fmt::Result {
    let noun = if line_count == 1 { "line" } else { "lines" };
    write!(out, "[Pasted text #{id}, {line_count} {noun}]")
}

struct PlaceholderMatch<'a> {
    remaining: &'a str,
}

impl Write for PlaceholderMatch<'_> {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        self.remaining = self.remaining.strip_prefix(piece).ok_or(fmt::Error)?;
        Ok(())
    }
}

fn is_placeholder(raw: &str, id: usize, line_count: usize) -> bool {
    let mut matcher = PlaceholderMatch { remaining: raw };
    write_placeholder(&mut matcher, id, line_count).is_ok() && matcher.remaining.is_empty()
}

pub(crate) fn expand<'a>(text: &'a str, blocks: &[PastedBlock]) -> Cow<'a, str> {
    expand_range(text, blocks, 0, text.len()).unwrap_or(Cow::Borrowed(text))
}

pub(crate) fn expand_range<'a>(
    text: &'a str,
    blocks: &[PastedBlock],
    range_start: usize,
    range_end: usize,
) -> Option<Cow<'a, str>> {
    if range_start > range_end || range_end > text.len() {
        return None;
    }
    let mut out = String::new();
    let mut expanded_any = false;
    let mut index = range_start;
    let mut copy_from = range_start;
    while index < range_end {
        if let Some(block) = registered_block_starting_at(text, blocks, index)
            && block.span.raw_end <= range_end
        {
            out.push_str(&text[copy_from..index]);
            out.push_str(&block.text);
            expanded_any = true;
            index = block.span.raw_end;
            copy_from = index;
            continue;
        }
        index = next_char_boundary(text, index);
    }
    if !expanded_any {
        return text.get(range_start..range_end).map(Cow::Borrowed);
    }
    out.push_str(&text[copy_from..range_end]);
    Some(Cow::Owned(out))
}

pub(crate) fn expanded_range_len(
    text: &str,
    blocks: &[PastedBlock],
    range_start: usize,
    range_end: usize,
) -> Option<usize> {
    if range_start > range_end || range_end > text.len() {
        return None;
    }
    let mut expanded_len = 0_usize;
    let mut index = range_start;
    while index < range_end {
        if let Some(block) = registered_block_starting_at(text, blocks, index)
            && block.span.raw_end <= range_end
        {
            expanded_len = expanded_len.checked_add(block.text.len())?;
            index = block.span.raw_end;
            continue;
        }
        let next = next_char_boundary(text, index);
        expanded_len = expanded_len.checked_add(next - index)?;
        index = next;
    }
    Some(expanded_len)
}

pub(crate) fn expanded_len(text: &str, blocks: &[PastedBlock]) -> Option<usize> {
    let mut expanded_len = text.len();
    for block in blocks {
        if !is_registered_block(text, block) {
            continue;
        }
        let placeholder_len = block.span.raw_end - block.span.raw_start;
        expanded_len = if block.text.len() >= placeholder_len {
            expanded_len.checked_add(block.text.len() - placeholder_len)?
        } else {
            expanded_len.checked_sub(placeholder_len - block.text.len())?
        };
    }
    Some(expanded_len)
}

pub(crate) fn registered_placeholder_span_starting_at(
    text: &str,
    raw_start: usize,
    blocks: &[PastedBlock],
) -> Option<Span> {
    registered_block_starting_at(text, blocks, raw_start).map(|block| block.span)
}

fn registered_block_starting_at<'a>(
    text: &str,
    blocks: &'a [PastedBlock],
    raw_start: usize,
) -> Option<&'a PastedBlock> {
    let block = blocks
        .iter()
        .find(|block| block.span.raw_start >= raw_start)?;
    (block.span.raw_start == raw_start && is_registered_block(text, block)).then_some(block)
}

fn is_registered_block(text: &str, block: &PastedBlock) -> bool {
    block.span.is_valid(text.len())
        && text
            .get(block.span.raw_start..block.span.raw_end)
            .is_some_and(|raw| is_placeholder(raw, block.id, block.line_count))
}

fn next_char_boundary(text: &str, index: usize) -> usize {
    text.get(index..)
        .and_then(|rest| rest.chars().next())
        .map_or(index + 1, |character| index + character.len_utf8())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(id: usize, text: &str, line_count: usize, start: usize, end: usize) -> PastedBlock {
        PastedBlock {
            id,
            text: text.to_owned(),
            line_count,
            span: Span::new(start, end),
        }
    }

    #[test]
    fn count_lines_counts_lines_with_and_without_trailing_newline() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("foo"), 1);
        assert_eq!(count_lines("foo\n"), 1);
        assert_eq!(count_lines("foo\nbar"), 2);
        assert_eq!(count_lines("foo\nbar\n"), 2);
        assert_eq!(count_lines("a\nb\nc"), 3);
    }

    #[test]
    fn format_placeholder_singular_and_plural() {
        assert_eq!(format_placeholder(1, 1), "[Pasted text #1, 1 line]");
        assert_eq!(format_placeholder(42, 17), "[Pasted text #42, 17 lines]");
    }

    #[test]
    fn placeholders_match_in_place_only_when_identical() {
        for (id, line_count) in [(1, 1), (42, 17), (7, 0), (usize::MAX, 1_000_000)] {
            let placeholder = format_placeholder(id, line_count);
            assert!(is_placeholder(&placeholder, id, line_count));
            assert!(!is_placeholder(
                &placeholder[..placeholder.len() - 1],
                id,
                line_count
            ));
            assert!(!is_placeholder(&format!("{placeholder} "), id, line_count));
            assert!(!is_placeholder(
                &placeholder,
                id.wrapping_add(1),
                line_count
            ));
            assert!(!is_placeholder(&placeholder, id, line_count + 1));
        }
        assert!(!is_placeholder("", 1, 1));
    }

    #[test]
    fn should_use_placeholder_matches_large_paste_threshold() {
        assert!(!should_use_placeholder("hello"));
        assert!(!should_use_placeholder(
            &"x".repeat(LARGE_PASTE_CHAR_THRESHOLD)
        ));
        assert!(should_use_placeholder(
            &"x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1)
        ));
        assert!(!should_use_placeholder(
            &"\u{e9}".repeat(LARGE_PASTE_CHAR_THRESHOLD)
        ));
        assert!(should_use_placeholder(
            &"\u{e9}".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1)
        ));
    }

    #[test]
    fn expand_substitutes_placeholder_for_real_text() {
        let input = "prefix [Pasted text #1, 2 lines] suffix";
        let start = "prefix ".len();
        let blocks = [block(
            1,
            "hello\nworld",
            2,
            start,
            start + "[Pasted text #1, 2 lines]".len(),
        )];
        let result = expand(input, &blocks);
        assert_eq!(result, "prefix hello\nworld suffix");
        assert!(matches!(result, Cow::Owned(_)));
    }

    #[test]
    fn expand_with_no_placeholder_returns_original_slice_unowned() {
        let result = expand("nothing to replace", &[]);
        assert!(matches!(result, Cow::Borrowed("nothing to replace")));
    }

    #[test]
    fn expand_preserves_unknown_placeholder_ids_verbatim() {
        let input = "before [Pasted text #9, 4 lines] [Pasted text #1, 1 line]";
        let blocks = [block(
            1,
            "X",
            1,
            "before [Pasted text #9, 4 lines] ".len(),
            input.len(),
        )];
        assert_eq!(expand(input, &blocks), "before [Pasted text #9, 4 lines] X");
    }

    #[test]
    fn expanded_lengths_count_registered_backing_text_without_allocating() {
        let placeholder = "[Pasted text #1, 1 line]";
        let input = format!("ab{placeholder}cd");
        let blocks = [block(1, "expanded", 1, 2, 2 + placeholder.len())];
        assert_eq!(expanded_len(&input, &blocks), Some("abexpandedcd".len()));
        assert_eq!(
            expanded_range_len(&input, &blocks, 2, 2 + placeholder.len()),
            Some("expanded".len())
        );
    }

    #[test]
    fn expand_ignores_typed_lookalikes_with_a_registered_id() {
        let registered = "[Pasted text #1, 1 line]";
        let input = format!("{registered} typed [Pasted text #1, 999 lines]");
        let blocks = [block(1, "original", 1, 0, registered.len())];
        assert_eq!(
            expand(&input, &blocks),
            "original typed [Pasted text #1, 999 lines]"
        );
    }

    #[test]
    fn expand_range_keeps_partial_placeholders_literal() {
        let placeholder = "[Pasted text #1, 1 line]";
        let input = format!("ab{placeholder}cd");
        let blocks = [block(1, "expanded", 1, 2, 2 + placeholder.len())];
        assert_eq!(expand_range(&input, &blocks, 0, 5).unwrap(), "ab[Pa");
        assert_eq!(
            expand_range(&input, &blocks, 1, input.len()).unwrap(),
            "bexpandedcd"
        );
        assert_eq!(expand_range(&input, &blocks, 5, 4), None);
    }
}
