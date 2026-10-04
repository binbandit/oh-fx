use ofx_text::{display_unit_at, prefix_by_width, visible_width};

use crate::row_text::Row;
use crate::theme::Theme;

const DRAFT_SEPARATOR: &str = ", ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Amending<'a> {
    pub(crate) draft: &'a str,
    pub(crate) cursor: usize,
    pub(crate) placeholder: &'static str,
}

pub(crate) fn push_amending_label(
    row: &mut Row,
    theme: &Theme,
    label: &str,
    amending: Amending<'_>,
    width: usize,
) {
    let prefix = format!("{label}{DRAFT_SEPARATOR}");
    let prefix = prefix_by_width(&prefix, width);
    row.push(prefix, theme.tag);
    let room = width.saturating_sub(visible_width(prefix));
    if room == 0 {
        return;
    }
    if amending.draft.is_empty() {
        let placeholder = prefix_by_width(amending.placeholder, room);
        let first = placeholder.chars().next().map_or(0, char::len_utf8);
        row.push(&placeholder[..first], theme.statusline.with_reverse());
        row.push(&placeholder[first..], theme.statusline);
        return;
    }
    let (before, cursor, after) = draft_window(amending.draft, amending.cursor, room);
    row.push(before, theme.tag);
    row.push(
        if cursor.is_empty() { " " } else { cursor },
        theme.tag.with_reverse(),
    );
    row.push(after, theme.tag);
}

fn draft_window(draft: &str, cursor: usize, max_width: usize) -> (&str, &str, &str) {
    let cursor = cursor.min(draft.len());
    let unit = display_unit_at(draft, cursor);
    let show_unit = unit.byte_len > 0 && unit.cell_width > 0 && unit.cell_width <= max_width;
    let (cursor_width, cursor_end) = if show_unit {
        (unit.cell_width, cursor + unit.byte_len)
    } else {
        (1, cursor)
    };
    let mut before_start = 0;
    let mut before_width = visible_width(&draft[..cursor]);
    while before_start < cursor && before_width + cursor_width > max_width {
        let leading = display_unit_at(draft, before_start);
        before_width = before_width.saturating_sub(leading.cell_width);
        before_start += leading.byte_len.max(1);
    }
    let after_width = max_width.saturating_sub(before_width + cursor_width);
    (
        &draft[before_start..cursor],
        &draft[cursor..cursor_end],
        prefix_by_width(&draft[cursor_end..], after_width),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_keeps_the_cursor_and_drops_text_from_the_front() {
        assert_eq!(draft_window("abcdef", 6, 4), ("def", "", ""));
        assert_eq!(draft_window("abcdef", 0, 4), ("", "a", "bcd"));
        assert_eq!(draft_window("abcdef", 3, 3), ("bc", "d", ""));
        assert_eq!(draft_window("ab文字", 2, 4), ("ab", "文", ""));
        assert_eq!(draft_window("文字x", 3, 2), ("", "字", ""));
    }
}
