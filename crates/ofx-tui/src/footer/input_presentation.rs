use std::borrow::Cow;

use ofx_text::{prefix_by_width, visible_width};

use crate::composer::{Composer, LayoutEvent, UnitKind, terminal_column, visible_window};
use crate::row_text::{Paint, Row, escaped_in_rows};
use crate::theme::Theme;

const CTRL_C_EXIT_HINT: &str = "press ctrl+c again to exit";
const ESC_CLEAR_HINT: &str = "esc again to clear";
const ESC_INTERRUPT_HINTS: [&str; 3] = ["esc again to interrupt", "esc esc interrupt", "esc esc"];
const ESC_INTERRUPT_FALLBACK: &str = "esc esc to interrupt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposerView {
    pub(crate) rows: Vec<Row>,
    pub(crate) cursor: (usize, usize),
}

pub(crate) fn input_row_limit(content_bottom: usize) -> usize {
    (if content_bottom > 4 {
        content_bottom / 2
    } else {
        0
    }) + 1
}

pub(crate) fn composer_view(
    composer: &Composer,
    cols: u16,
    row_limit: usize,
    theme: &Theme,
) -> ComposerView {
    let input = composer.text();
    let layout = composer.visual_layout(cols);
    let summary = layout.summary(None);
    let window = visible_window(summary.cursor.row_index, summary.total_rows, row_limit);
    let end_row = window.first_row + window.row_count;
    let selection = composer.selection();
    let width = usize::from(cols);
    let mut rows = Vec::new();
    let mut current: Option<(Row, usize)> = None;
    for event in layout.events() {
        let row_index = match event {
            LayoutEvent::Unit(unit) => unit.row_index,
            LayoutEvent::RowEnd(row) => row.index,
        };
        if row_index < window.first_row {
            continue;
        }
        if row_index >= end_row {
            break;
        }
        let (row, remaining) = current.get_or_insert_with(|| {
            start_row(
                theme,
                width,
                row_index == window.first_row && window.first_row > 0,
            )
        });
        match event {
            LayoutEvent::Unit(unit) => {
                if *remaining == 0 {
                    continue;
                }
                let selected = selection
                    .is_some_and(|range| unit.raw_start < range.end && unit.raw_end > range.start);
                let paint = if selected {
                    Paint::PLAIN.with_reverse()
                } else {
                    Paint::PLAIN
                };
                let text = &input[unit.raw_start..unit.raw_end];
                match unit.kind {
                    UnitKind::Tab => {
                        let spaces = unit.cell_width.min(*remaining);
                        row.push(&" ".repeat(spaces), paint);
                        *remaining -= spaces;
                    }
                    UnitKind::Text | UnitKind::PastePlaceholder => {
                        let visible = drawable(prefix_by_width(text, *remaining));
                        row.push(&visible, paint);
                        *remaining -= visible_width(&visible);
                    }
                }
            }
            LayoutEvent::RowEnd(_) => {
                if let Some((row, _)) = current.take() {
                    rows.push(row);
                }
            }
        }
    }
    if rows.is_empty() {
        rows.push(start_row(theme, width, false).0);
    }
    let cursor_row = summary
        .cursor
        .row_index
        .saturating_sub(window.first_row)
        .min(rows.len() - 1);
    let cursor_col = usize::from(terminal_column(summary.cursor, cols)).saturating_sub(1);
    ComposerView {
        rows,
        cursor: (cursor_row, cursor_col),
    }
}

fn drawable(text: &str) -> Cow<'_, str> {
    if text.contains(escaped_in_rows) {
        Cow::Owned(
            text.chars()
                .filter(|character| !escaped_in_rows(*character))
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

fn start_row(theme: &Theme, width: usize, hidden_above: bool) -> (Row, usize) {
    let rail = if hidden_above { "┃↑" } else { "┃" };
    let mut row = Row::styled(rail, theme.hint).clipped(width);
    if !hidden_above && width > 1 {
        row.push(" ", Paint::PLAIN);
    }
    (row, width.saturating_sub(2))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HintState {
    pub(crate) ctrl_c_pending: bool,
    pub(crate) esc_clear_armed: bool,
    pub(crate) esc_interrupt_armed: bool,
}

pub(crate) fn compose_hint_row(theme: &Theme, base: &Row, state: HintState, width: usize) -> Row {
    let mut left = if state.ctrl_c_pending {
        Row::styled(CTRL_C_EXIT_HINT, theme.statusline)
    } else {
        base.clone()
    };
    let mut right = "";
    if state.esc_interrupt_armed {
        let left_width = left.width();
        match ESC_INTERRUPT_HINTS
            .iter()
            .find(|candidate| width > left_width + visible_width(candidate))
        {
            Some(candidate) => right = candidate,
            None => left = Row::styled(ESC_INTERRUPT_FALLBACK, theme.statusline),
        }
    } else if state.esc_clear_armed {
        right = ESC_CLEAR_HINT;
    }
    let right_width = visible_width(right);
    let left_width = if right_width > 0 && width > right_width {
        width - right_width - 1
    } else {
        width
    };
    let mut row = left.clipped(left_width);
    let hint_width = row.width();
    if right_width > 0 && width > hint_width + right_width {
        row.push_spaces(width - right_width - hint_width);
        row.push(right, theme.dim);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{MoveIntent, MoveKind};

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    fn composer(text: &str) -> Composer {
        let mut composer = Composer::new();
        composer.insert_text(text, usize::MAX);
        composer
    }

    fn texts(view: &ComposerView) -> Vec<String> {
        view.rows.iter().map(Row::text).collect()
    }

    #[test]
    fn composer_rows_use_a_rail_on_every_visual_row() {
        let view = composer_view(&composer(""), 40, 5, &theme());
        assert_eq!(texts(&view), ["┃ "]);
        assert_eq!(view.cursor, (0, 2));
        let view = composer_view(
            &composer("line one\nline two is quite a bit longer than the width"),
            40,
            5,
            &theme(),
        );
        assert_eq!(
            texts(&view),
            [
                "┃ line one",
                "┃ line two is quite a bit longer than ",
                "┃ the width"
            ]
        );
        assert_eq!(view.cursor, (2, 11));
        assert_eq!(view.rows[0].segments()[0].paint, Paint::fg(255));
    }

    #[test]
    fn invisible_controls_take_no_cells_where_the_layout_gives_them_none() {
        let view = composer_view(&composer("ab\u{85}\u{9b}c\u{202e}d"), 40, 5, &theme());
        assert_eq!(texts(&view), ["┃ abcd"]);
        assert_eq!(view.cursor, (0, 6));
        assert_eq!(view.rows[0].width(), 6);
    }

    #[test]
    fn composer_rows_follow_the_cursor_and_mark_clipped_rows() {
        let view = composer_view(&composer("a\nb\nc\nd"), 40, 2, &theme());
        assert_eq!(texts(&view), ["┃↑c", "┃ d"]);
        assert_eq!(view.cursor, (1, 3));
        assert_eq!(input_row_limit(26), 14);
        assert_eq!(input_row_limit(4), 1);
    }

    #[test]
    fn selections_render_in_reverse_video() {
        let mut composer = composer("hello");
        composer.move_cursor(MoveIntent {
            kind: MoveKind::WordLeft,
            extend_selection: true,
        });
        let view = composer_view(&composer, 40, 5, &theme());
        assert!(
            view.rows[0]
                .segments()
                .iter()
                .any(|segment| segment.paint.has(crate::row_text::Attribute::Reverse))
        );
    }

    #[test]
    fn the_hint_row_right_aligns_escape_cues_and_replaces_for_ctrl_c() {
        let base = Row::styled("auto · fake-model", Paint::fg(245));
        let idle = HintState {
            ctrl_c_pending: false,
            esc_clear_armed: false,
            esc_interrupt_armed: false,
        };
        assert_eq!(
            compose_hint_row(&theme(), &base, idle, 100).text(),
            "auto · fake-model"
        );
        let clear = compose_hint_row(
            &theme(),
            &base,
            HintState {
                esc_clear_armed: true,
                ..idle
            },
            100,
        );
        assert_eq!(clear.width(), 100);
        assert!(clear.text().ends_with(" esc again to clear"));
        let interrupt = compose_hint_row(
            &theme(),
            &base,
            HintState {
                esc_interrupt_armed: true,
                ..idle
            },
            30,
        );
        assert_eq!(interrupt.text(), "auto · fake-model      esc esc");
        let narrow = compose_hint_row(
            &theme(),
            &base,
            HintState {
                esc_interrupt_armed: true,
                ..idle
            },
            20,
        );
        assert_eq!(narrow.text(), "esc esc to interrupt");
        let ctrl_c = compose_hint_row(
            &theme(),
            &base,
            HintState {
                ctrl_c_pending: true,
                ..idle
            },
            100,
        );
        assert_eq!(ctrl_c.text(), "press ctrl+c again to exit");
    }
}
