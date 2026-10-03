use ofx_text::{display_unit_at, visible_width};

use super::question_freeform_layout::{
    OPTION_ROW_INDENT, content_width, next_line, normalized_cursor, option_prefix_width, ordinal,
};
use crate::row_text::{Paint, Row, escaped_prefix_by_width, escaped_width};
use crate::shell::question_prompt::{EntryView, PromptOption, PromptView};
use crate::theme::Theme;

const QUESTION_INDENT: &str = "  ";
const FREEFORM_FULL_HINT: &str =
    "type answer    ↑↓←→ cursor    shift+↑↓ options    tab questions    enter answer    esc cancel";
const FREEFORM_HINTS: [&str; 4] = [
    "↑↓ cursor · shift+↑↓ options · tab questions · enter answer · esc cancel",
    "shift+↑↓ options · tab questions · enter answer · esc cancel",
    "tab questions · enter answer · esc cancel",
    "enter answer · esc cancel",
];
const PREDEFINED_HINTS: [&str; 3] = [
    "↑↓ options · tab questions · enter answer · esc cancel",
    "tab questions · enter answer · esc cancel",
    "enter answer · esc cancel",
];
const COUNTER_GAP: usize = 2;
const WIDE_DESCRIPTION_MIN_COLS: usize = 72;
const WIDE_DESCRIPTION_COLUMN: usize = 38;
const NARROW_DESCRIPTION_MIN_COLS: usize = 52;
const NARROW_DESCRIPTION_COLUMN: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wrapped {
    content_end: usize,
    next_start: usize,
}

struct OptionStyle<'a> {
    theme: &'a Theme,
    paint: Paint,
    ordinal: String,
    prefix_width: usize,
}

impl OptionStyle<'_> {
    fn start_row(&self, first: bool) -> Row {
        if first {
            let mut row = Row::styled(OPTION_ROW_INDENT, self.paint);
            row.push(&self.ordinal, self.paint);
            row
        } else {
            let mut row = Row::new();
            row.push_spaces(self.prefix_width);
            row
        }
    }
}

pub(crate) fn question_panel_rows(theme: &Theme, entry: EntryView<'_>, cols: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let question_width = cols.saturating_sub(QUESTION_INDENT.len()).max(1);
    for_each_line(entry.question, question_width, question_width, |line| {
        let mut row = Row::plain(QUESTION_INDENT);
        row.push(line, Paint::PLAIN.with_bold());
        rows.push(row);
    });
    rows.push(Row::new());
    for (index, option) in entry.options.iter().enumerate() {
        let selected = index == entry.choice;
        let style = OptionStyle {
            theme,
            paint: if selected {
                theme.selected_completion
            } else {
                theme.dim
            },
            ordinal: ordinal(index),
            prefix_width: option_prefix_width(index),
        };
        if selected && option.freeform {
            selected_freeform_rows(&style, entry, cols, &mut rows);
        } else {
            option_rows(&style, entry, option, cols, &mut rows);
        }
    }
    rows.into_iter().map(|row| row.clipped(cols)).collect()
}

fn option_label<'a>(entry: EntryView<'a>, option: &'a PromptOption) -> &'a str {
    if option.freeform && !entry.draft.text.is_empty() {
        &entry.draft.text
    } else {
        &option.label
    }
}

fn description_column(cols: usize) -> usize {
    if cols >= WIDE_DESCRIPTION_MIN_COLS {
        WIDE_DESCRIPTION_COLUMN
    } else if cols >= NARROW_DESCRIPTION_MIN_COLS {
        NARROW_DESCRIPTION_COLUMN
    } else {
        0
    }
}

fn option_rows(
    style: &OptionStyle<'_>,
    entry: EntryView<'_>,
    option: &PromptOption,
    cols: usize,
    rows: &mut Vec<Row>,
) {
    let label = option_label(entry, option);
    let description = option.description.as_deref().unwrap_or_default();
    let has_description = !description.is_empty();
    let column = if has_description {
        description_column(cols)
    } else {
        0
    };
    let prefix_width = style.prefix_width;
    let label_end = if column > prefix_width + 2 {
        column - 2
    } else {
        cols
    };
    let first_width = label_end.saturating_sub(prefix_width).max(1);
    if column > 0 {
        let description_width = cols.saturating_sub(column).max(1);
        let description_indent = column - 1;
        let mut label_start = 0;
        let mut description_start = 0;
        let mut row_index = 0;
        while row_index == 0 || label_start < label.len() || description_start < description.len() {
            let label_available = row_index == 0 || label_start < label.len();
            let label_line = if !label_available || label.is_empty() {
                Wrapped {
                    content_end: label_start,
                    next_start: label_start,
                }
            } else {
                next_wrapped_line(label, label_start, first_width)
            };
            let description_available = description_start < description.len();
            let description_line = if description_available {
                next_wrapped_line(description, description_start, description_width)
            } else {
                Wrapped {
                    content_end: description_start,
                    next_start: description_start,
                }
            };
            let mut row = style.start_row(row_index == 0);
            let piece = if label_available {
                &label[label_start..label_line.content_end]
            } else {
                ""
            };
            row.push(piece, style.paint);
            if description_available {
                let visible = prefix_width + escaped_width(piece);
                row.push_spaces(if visible < description_indent {
                    description_indent - visible
                } else {
                    1
                });
                row.push(
                    &description[description_start..description_line.content_end],
                    style.theme.dim,
                );
            }
            rows.push(row);
            if label_available {
                label_start = label_line.next_start;
            }
            if description_available {
                description_start = description_line.next_start;
            }
            row_index += 1;
        }
        return;
    }
    let continuation_width = cols.saturating_sub(prefix_width).max(1);
    let mut first = true;
    for_each_line(label, first_width, continuation_width, |piece| {
        let mut row = style.start_row(first);
        row.push(piece, style.paint);
        rows.push(row);
        first = false;
    });
    if has_description {
        let width = cols.saturating_sub(prefix_width).max(1);
        let mut start = 0;
        while start < description.len() {
            let line = next_wrapped_line(description, start, width);
            let mut row = style.start_row(false);
            row.push(&description[start..line.content_end], style.theme.dim);
            rows.push(row);
            start = line.next_start;
        }
    }
}

fn selected_freeform_rows(
    style: &OptionStyle<'_>,
    entry: EntryView<'_>,
    cols: usize,
    rows: &mut Vec<Row>,
) {
    let reverse = Paint::PLAIN.with_reverse();
    let buffer = entry.draft.text.as_str();
    let cursor = normalized_cursor(buffer, entry.draft.cursor);
    let width = content_width(entry.choice, cols);
    if buffer.is_empty() {
        let mut row = style.start_row(true);
        row.push(" ", reverse);
        rows.push(row);
        return;
    }
    let mut start = 0;
    let mut first = true;
    let mut last_width = 0;
    let mut trailing_hard_break = false;
    while start < buffer.len() {
        let line = next_line(buffer, start, width);
        let end = line.content_end;
        last_width = escaped_width(&buffer[start..end]);
        trailing_hard_break = line.hard_break && line.next_start == buffer.len();
        let mut row = style.start_row(first);
        if cursor >= start && cursor < end {
            row.push(&buffer[start..cursor], style.paint);
            let cursor_end = (cursor + display_unit_at(buffer, cursor).byte_len.max(1)).min(end);
            row.push(&buffer[cursor..cursor_end], reverse);
            row.push(&buffer[cursor_end..end], style.paint);
        } else {
            row.push(&buffer[start..end], style.paint);
        }
        let at_end = line.next_start == buffer.len() && !line.hard_break;
        if (cursor == end && line.hard_break)
            || (at_end && cursor == buffer.len() && last_width < width)
        {
            row.push(" ", reverse);
        }
        rows.push(row);
        first = false;
        start = line.next_start;
    }
    if trailing_hard_break || (cursor == buffer.len() && last_width == width) {
        let mut row = style.start_row(false);
        row.push(" ", reverse);
        rows.push(row);
    }
}

pub(crate) fn question_hint_row(theme: &Theme, view: &PromptView<'_>, width: usize) -> Row {
    let full = if view.freeform_selected() {
        FREEFORM_FULL_HINT.to_owned()
    } else {
        let options = view.entry.map_or(0, |entry| entry.options.len());
        format!(
            "1–{options} choose now    ↑↓ options    tab questions    enter answer    esc cancel"
        )
    };
    let variants: &[&str] = if view.freeform_selected() {
        &FREEFORM_HINTS
    } else {
        &PREDEFINED_HINTS
    };
    let base = if visible_width(&full) <= width {
        full
    } else {
        variants
            .iter()
            .find(|variant| visible_width(variant) <= width)
            .unwrap_or(&variants[variants.len() - 1])
            .to_string()
    };
    let mut row = Row::styled(&base, theme.dim);
    if view.count > 1 {
        let counter = format!("Question {} of {}", view.index + 1, view.count);
        let base_width = visible_width(&base);
        let counter_width = visible_width(&counter);
        if base_width + counter_width + COUNTER_GAP <= width {
            row.push_spaces(width - base_width - counter_width);
            row.push(&counter, theme.dim);
        }
    }
    row
}

pub(crate) fn resolution_rows(
    theme: &Theme,
    answers: &[(String, String)],
    cols: usize,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for (index, (question, answer)) in answers.iter().enumerate() {
        let prefix = format!("{QUESTION_INDENT}{}", ordinal(index));
        let indent = " ".repeat(visible_width(&prefix));
        resolution_field(&prefix, &indent, question, Paint::PLAIN, cols, &mut rows);
        resolution_field(&indent, &indent, answer, theme.statusline, cols, &mut rows);
    }
    rows.into_iter().map(|row| row.clipped(cols)).collect()
}

pub(crate) fn cancelled_resolution_row(theme: &Theme) -> Row {
    let mut row = Row::styled("■", theme.red);
    row.push(" Cancelled", Paint::PLAIN);
    row
}

fn resolution_field(
    first_prefix: &str,
    continuation_prefix: &str,
    text: &str,
    paint: Paint,
    cols: usize,
    rows: &mut Vec<Row>,
) {
    let mut start = 0;
    let mut first = true;
    while first || start < text.len() {
        let prefix = if first {
            first_prefix
        } else {
            continuation_prefix
        };
        let line = next_wrapped_line(text, start, cols.saturating_sub(visible_width(prefix)));
        let mut row = Row::styled(prefix, paint);
        row.push(&text[start..line.content_end], paint);
        rows.push(row);
        if text.is_empty() {
            break;
        }
        first = false;
        start = line.next_start;
    }
}

fn for_each_line(
    text: &str,
    first_width: usize,
    continuation_width: usize,
    mut emit: impl FnMut(&str),
) {
    if text.is_empty() {
        emit("");
        return;
    }
    let mut start = 0;
    let mut first = true;
    while start < text.len() {
        let width = if first {
            first_width
        } else {
            continuation_width
        };
        let line = next_wrapped_line(text, start, width);
        emit(&text[start..line.content_end]);
        first = false;
        start = line.next_start;
    }
}

fn next_wrapped_line(text: &str, start: usize, width: usize) -> Wrapped {
    if start >= text.len() {
        return Wrapped {
            content_end: text.len(),
            next_start: text.len(),
        };
    }
    let segment_end = text[start..]
        .find('\n')
        .map_or(text.len(), |relative| start + relative);
    let segment = &text[start..segment_end];
    let prefix = wrap_cut(segment, width);
    if prefix.len() < segment.len() {
        if prefix.is_empty() {
            let end = (start + display_unit_at(text, start).byte_len.max(1)).min(segment_end);
            return Wrapped {
                content_end: end,
                next_start: end,
            };
        }
        let content_end = start + prefix.len();
        let skipped = text[content_end..segment_end]
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        return Wrapped {
            content_end,
            next_start: content_end + skipped,
        };
    }
    Wrapped {
        content_end: segment_end,
        next_start: if segment_end < text.len() {
            segment_end + 1
        } else {
            segment_end
        },
    }
}

fn wrap_cut(segment: &str, width: usize) -> &str {
    let prefix = escaped_prefix_by_width(segment, width);
    if prefix.len() == segment.len() {
        return prefix;
    }
    match prefix.rfind([' ', '\t']) {
        Some(space) if space > 0 => &segment[..space],
        _ => prefix,
    }
}

#[cfg(test)]
mod tests;
