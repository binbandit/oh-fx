use ofx_text::{display_unit_at, prefix_by_width, visible_width};

pub(crate) const OPTION_ROW_INDENT: &str = "    ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) start: usize,
    pub(crate) content_end: usize,
    pub(crate) next_start: usize,
    pub(crate) hard_break: bool,
    synthetic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorMove {
    pub(crate) cursor: usize,
    pub(crate) preferred_column: usize,
    pub(crate) moved: bool,
}

pub(crate) fn ordinal(index: usize) -> String {
    format!("{}) ", index + 1)
}

pub(crate) fn option_prefix_width(index: usize) -> usize {
    visible_width(OPTION_ROW_INDENT) + visible_width(&ordinal(index))
}

pub(crate) fn content_width(choice_index: usize, row_budget: usize) -> usize {
    row_budget
        .saturating_sub(option_prefix_width(choice_index))
        .max(1)
}

pub(crate) fn normalized_cursor(buffer: &str, cursor: usize) -> usize {
    buffer.floor_char_boundary(cursor.min(buffer.len()))
}

pub(crate) fn next_line(buffer: &str, start: usize, content_width: usize) -> Line {
    debug_assert!(content_width > 0);
    if start >= buffer.len() {
        return Line {
            start: buffer.len(),
            content_end: buffer.len(),
            next_start: buffer.len(),
            hard_break: false,
            synthetic: true,
        };
    }
    let segment_end = buffer[start..]
        .find('\n')
        .map_or(buffer.len(), |relative| start + relative);
    let segment = &buffer[start..segment_end];
    let prefix = prefix_by_width(segment, content_width);
    if prefix.len() < segment.len() {
        let end = if prefix.is_empty() {
            (start + display_unit_at(buffer, start).byte_len).min(segment_end)
        } else {
            start + prefix.len()
        };
        return Line {
            start,
            content_end: end,
            next_start: end,
            hard_break: false,
            synthetic: false,
        };
    }
    let hard_break = segment_end < buffer.len();
    Line {
        start,
        content_end: segment_end,
        next_start: if hard_break {
            segment_end + 1
        } else {
            segment_end
        },
        hard_break,
        synthetic: false,
    }
}

pub(crate) fn move_cursor(
    buffer: &str,
    cursor: usize,
    content_width: usize,
    direction: Direction,
    preferred_column: Option<usize>,
) -> CursorMove {
    let safe_cursor = normalized_cursor(buffer, cursor);
    let mut previous: Option<Line> = None;
    let mut line = next_line(buffer, 0, content_width);
    loop {
        if line_owns_cursor(buffer, line, safe_cursor, content_width) {
            let column =
                preferred_column.unwrap_or_else(|| cursor_column(buffer, line, safe_cursor));
            let target = match direction {
                Direction::Up => previous,
                Direction::Down => advance_line(buffer, line, content_width),
            };
            return match target {
                Some(target) => CursorMove {
                    cursor: cursor_at_column(buffer, target, column, content_width),
                    preferred_column: column,
                    moved: true,
                },
                None => CursorMove {
                    cursor: safe_cursor,
                    preferred_column: column,
                    moved: false,
                },
            };
        }
        let Some(next) = advance_line(buffer, line, content_width) else {
            break;
        };
        previous = Some(line);
        line = next;
    }
    CursorMove {
        cursor: safe_cursor,
        preferred_column: preferred_column.unwrap_or(0),
        moved: false,
    }
}

fn advance_line(buffer: &str, line: Line, content_width: usize) -> Option<Line> {
    if line.synthetic {
        return None;
    }
    if line.next_start < buffer.len() {
        return Some(next_line(buffer, line.next_start, content_width));
    }
    if line.hard_break && line.next_start == buffer.len() {
        return Some(next_line(buffer, buffer.len(), content_width));
    }
    if !line.hard_break
        && line.content_end == buffer.len()
        && visible_width(&buffer[line.start..line.content_end]) == content_width
    {
        return Some(next_line(buffer, buffer.len(), content_width));
    }
    None
}

fn line_owns_cursor(buffer: &str, line: Line, cursor: usize, content_width: usize) -> bool {
    if line.synthetic {
        return cursor == line.start;
    }
    if cursor < line.start || cursor > line.content_end {
        return false;
    }
    cursor < line.content_end
        || line.hard_break
        || advance_line(buffer, line, content_width).is_none()
}

fn cursor_column(buffer: &str, line: Line, cursor: usize) -> usize {
    if line.synthetic {
        return 0;
    }
    visible_width(&buffer[line.start..cursor])
}

fn cursor_at_column(buffer: &str, line: Line, target_column: usize, content_width: usize) -> usize {
    if line.synthetic || target_column == 0 {
        return line.start;
    }
    let mut cursor = line.start;
    let mut column = 0;
    let mut last_owned = line.start;
    while cursor < line.content_end {
        let unit = display_unit_at(buffer, cursor);
        if column + unit.cell_width > target_column {
            break;
        }
        cursor += unit.byte_len;
        column += unit.cell_width;
        if line_owns_cursor(buffer, line, cursor, content_width) {
            last_owned = cursor;
        }
        if column >= target_column {
            break;
        }
    }
    last_owned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_movement_preserves_preferred_column_across_hard_lines() {
        let buffer = "abcd\nef\nghij";
        let first = move_cursor(buffer, 11, 20, Direction::Up, None);
        assert_eq!(first.cursor, 7);
        assert_eq!(first.preferred_column, 3);
        let second = move_cursor(
            buffer,
            first.cursor,
            20,
            Direction::Up,
            Some(first.preferred_column),
        );
        assert_eq!(second.cursor, 3);
        let third = move_cursor(
            buffer,
            second.cursor,
            20,
            Direction::Down,
            Some(second.preferred_column),
        );
        assert_eq!(third.cursor, 7);
    }

    #[test]
    fn vertical_movement_follows_soft_wrapped_rows() {
        let buffer = "abcdefghijkl";
        let first = move_cursor(buffer, 10, 4, Direction::Up, None);
        assert_eq!(first.cursor, 6);
        let second = move_cursor(
            buffer,
            first.cursor,
            4,
            Direction::Up,
            Some(first.preferred_column),
        );
        assert_eq!(second.cursor, 2);
    }

    #[test]
    fn vertical_movement_stays_within_display_unit_boundaries() {
        let buffer = "a🙂b\nxy";
        let moved = move_cursor(buffer, buffer.len(), 20, Direction::Up, None);
        assert_eq!(moved.cursor, 1);
        let combining = "e\u{301}x\nab";
        let moved = move_cursor(combining, combining.len(), 20, Direction::Up, None);
        assert_eq!(moved.cursor, 4);
        assert!(combining.is_char_boundary(moved.cursor));
    }

    #[test]
    fn vertical_movement_is_inert_beyond_the_first_and_last_row() {
        let buffer = "one\ntwo";
        let up = move_cursor(buffer, 2, 20, Direction::Up, None);
        assert_eq!(up.cursor, 2);
        assert!(!up.moved);
        let down = move_cursor(buffer, buffer.len(), 20, Direction::Down, None);
        assert_eq!(down.cursor, buffer.len());
        assert!(!down.moved);
    }

    #[test]
    fn option_prefixes_grow_with_their_ordinal() {
        assert_eq!(option_prefix_width(0), 7);
        assert_eq!(option_prefix_width(9), 8);
        assert_eq!(content_width(0, 24), 17);
        assert_eq!(content_width(0, 3), 1);
    }
}
