use ofx_contract::{StatuslineItem, StatuslineToggles};
use ofx_text::{prefix_by_width, visible_width};

use crate::row_text::{Row, single_line_ellipsized};
use crate::theme::Theme;

const TITLE: &str = "Status line";
const TITLE_ROWS: usize = 2;
const COLUMN_GAP: usize = 4;
const INDENT: usize = 2;
const OPTIONS: [&str; 2] = ["off", "on"];
const OPTION_GAP: usize = 2;
const HINTS: [&str; 3] = [
    "↑↓ navigate     ←→ change     esc close",
    "↑↓ move  ←→ change  esc",
    "←→ esc",
];
pub(crate) const STATUSLINE_CHOICES: [(StatuslineItem, &str); 3] = [
    (StatuslineItem::Context, "Context"),
    (StatuslineItem::Session, "Session"),
    (StatuslineItem::Workspace, "Workspace"),
];

pub(crate) fn desired_row_count() -> usize {
    TITLE_ROWS + STATUSLINE_CHOICES.len()
}

pub(crate) fn statusline_menu_rows(
    theme: &Theme,
    toggles: StatuslineToggles,
    selected: usize,
    visible_rows: usize,
    width: usize,
) -> Vec<Row> {
    let count = STATUSLINE_CHOICES.len();
    let selected = selected % count;
    let choice = |index: usize| choice_row(theme, toggles, index, index == selected, width);
    match visible_rows {
        0 => Vec::new(),
        1 => vec![choice(selected)],
        _ => {
            let first_choice_row = if visible_rows == 2 { 1 } else { TITLE_ROWS };
            let visible_choices = (visible_rows - first_choice_row).min(count);
            let window_start = selected
                .saturating_sub(visible_choices - 1)
                .min(count - visible_choices);
            let mut rows = vec![Row::styled(
                prefix_by_width(TITLE, width),
                theme.selected_completion,
            )];
            if first_choice_row == TITLE_ROWS {
                rows.push(Row::new());
            }
            rows.extend((window_start..window_start + visible_choices).map(choice));
            rows
        }
    }
}

pub(crate) fn statusline_menu_hint_row(theme: &Theme, width: usize) -> Row {
    let hint = HINTS
        .into_iter()
        .find(|hint| visible_width(hint) <= width)
        .unwrap_or(HINTS[HINTS.len() - 1]);
    Row::styled(hint, theme.dim).clipped(width)
}

fn choice_row(
    theme: &Theme,
    toggles: StatuslineToggles,
    index: usize,
    selected: bool,
    width: usize,
) -> Row {
    let (item, label) = STATUSLINE_CHOICES[index];
    let indent = if width <= INDENT { 0 } else { INDENT };
    let value_column = value_column(width);
    let mut row = Row::new();
    row.push_spaces(indent);
    let label_paint = if selected {
        theme.selected_completion
    } else {
        theme.dim
    };
    let room = value_column.saturating_sub(indent + 2);
    row.push(&single_line_ellipsized(label, room), label_paint);
    row.push_spaces(value_column.saturating_sub(row.width()));
    let current = if toggles.enabled(item) { "on" } else { "off" };
    for (option_index, option) in OPTIONS.into_iter().enumerate() {
        let before = row.width();
        if before >= width {
            break;
        }
        if option_index > 0 {
            row.push_spaces(OPTION_GAP.min(width - before));
        }
        let paint = if option == current {
            theme.selected_completion
        } else {
            theme.dim
        };
        let room = width.saturating_sub(row.width());
        row.push(&single_line_ellipsized(option, room), paint);
        if row.width() >= width {
            break;
        }
    }
    row
}

fn value_column(width: usize) -> usize {
    let indent = if width <= INDENT { 0 } else { INDENT };
    let widest = STATUSLINE_CHOICES
        .iter()
        .map(|(_, label)| visible_width(label))
        .max()
        .unwrap_or(0);
    (indent + widest + COLUMN_GAP).min(width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_text::Paint;

    fn theme() -> Theme {
        Theme::builtin(false, true, true)
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    #[test]
    fn the_menu_aligns_every_value_after_the_widest_label() {
        let rows = statusline_menu_rows(&theme(), StatuslineToggles::default(), 0, 5, 80);
        assert_eq!(
            texts(&rows),
            [
                "Status line",
                "",
                "  Context      off  on",
                "  Session      off  on",
                "  Workspace    off  on",
            ]
        );
    }

    #[test]
    fn the_current_value_and_the_selected_label_are_highlighted() {
        let mut toggles = StatuslineToggles::default();
        toggles.set(StatuslineItem::Session, true);
        let theme = theme();
        let rows = statusline_menu_rows(&theme, toggles, 1, 5, 80);
        let session = &rows[3];
        let painted: Vec<(String, Paint)> = session
            .segments()
            .iter()
            .filter(|segment| !segment.text.trim().is_empty())
            .map(|segment| (segment.text.clone(), segment.paint))
            .collect();
        assert_eq!(
            painted,
            [
                ("Session".to_owned(), theme.selected_completion),
                ("off".to_owned(), theme.dim),
                ("on".to_owned(), theme.selected_completion),
            ]
        );
        let context = &rows[2];
        assert_eq!(context.segments()[1].paint, theme.dim);
    }

    #[test]
    fn short_and_narrow_menus_keep_the_selection_in_view_within_the_width() {
        let toggles = StatuslineToggles::default();
        assert_eq!(
            texts(&statusline_menu_rows(&theme(), toggles, 2, 1, 80)),
            ["  Workspace    off  on"]
        );
        assert_eq!(
            texts(&statusline_menu_rows(&theme(), toggles, 2, 2, 80)),
            ["Status line", "  Workspace    off  on"]
        );
        assert_eq!(
            texts(&statusline_menu_rows(&theme(), toggles, 2, 3, 80)),
            ["Status line", "", "  Workspace    off  on"]
        );
        for width in 1..=24 {
            for row in statusline_menu_rows(&theme(), toggles, 0, 5, width) {
                assert!(row.width() <= width, "{width}: {:?}", row.text());
            }
        }
        assert_eq!(
            statusline_menu_hint_row(&theme(), 30).text(),
            "↑↓ move  ←→ change  esc"
        );
    }
}
