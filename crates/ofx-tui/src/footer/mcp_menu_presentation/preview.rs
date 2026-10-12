use ofx_text::{display_unit_at, prefix_by_width, trim_break_whitespace};

use super::{TEXT_INDENT, text_row};
use crate::row_text::Row;
use crate::theme::Theme;

const LOADING: &str = "Loading MCP content…";

pub(crate) fn preview_visual_row_count(preview: Option<&str>, width: usize) -> usize {
    let Some(text) = preview else {
        return 1;
    };
    let content_width = width.saturating_sub(TEXT_INDENT);
    if content_width == 0 {
        return 1;
    }
    text.split('\n')
        .map(|line| chunks(line, content_width).count().max(1))
        .sum()
}

pub(super) fn preview_row(
    theme: &Theme,
    preview: Option<&str>,
    row_index: usize,
    width: usize,
    visible_rows: usize,
    window_start: usize,
) -> Row {
    let total = preview_visual_row_count(preview, width);
    let start = window_start.min(total - visible_rows.min(total));
    visual_row(preview.unwrap_or(LOADING), start + row_index, width).map_or_else(Row::new, |line| {
        text_row(line, width, theme.dim, TEXT_INDENT)
    })
}

fn visual_row(text: &str, target: usize, width: usize) -> Option<&str> {
    let content_width = width.saturating_sub(TEXT_INDENT);
    if content_width == 0 {
        return Some("");
    }
    text.split('\n')
        .flat_map(|line| {
            let empty = line.is_empty().then_some("");
            empty.into_iter().chain(chunks(line, content_width))
        })
        .nth(target)
}

fn chunks(line: &str, content_width: usize) -> impl Iterator<Item = &str> {
    let mut remaining = line;
    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }
        let chunk = chunk(remaining, content_width);
        remaining = trim_break_whitespace(&remaining[chunk.len()..]);
        Some(chunk)
    })
}

fn chunk(remaining: &str, content_width: usize) -> &str {
    let prefix = prefix_by_width(remaining, content_width);
    if prefix.len() == remaining.len() {
        return prefix;
    }
    let mut last_break = None;
    let mut saw_content = false;
    let mut index = 0;
    while index < prefix.len() {
        let byte = prefix.as_bytes()[index];
        if byte == b' ' || byte == b'\t' {
            if saw_content {
                last_break = Some(index);
            }
        } else {
            saw_content = true;
        }
        index += display_unit_at(prefix, index).byte_len.max(1);
    }
    if let Some(index) = last_break {
        return &remaining[..index];
    }
    if !prefix.is_empty() {
        return prefix;
    }
    let first = remaining.chars().next().map_or(1, char::len_utf8);
    &remaining[..first]
}
