use ofx_contract::{SettingCategory, SettingItem, SettingsSnapshot};
use ofx_text::visible_width;

use crate::row_text::{Row, single_line_ellipsized, terminal_safe};
use crate::theme::Theme;

const HEADER_ROWS: usize = 2;
const SETTING_ROWS: usize = 9;
const INDENT: usize = 2;
const COLUMN_GAP: usize = 4;
const OPTION_GAP: usize = 2;
const EMPTY: &str = "No settings found.";
const CTRL_C_EXIT_HINT: &str = "press ctrl+c again to exit";
const HINTS: [&str; 5] = [
    "↑↓ navigate     tab category     ←→ change     esc close",
    "↑↓ navigate  tab category  ←→ change  esc close",
    "↑↓ move  tab category  ←→ change  esc",
    "tab category  ←→ change  esc",
    "tab ←→ esc",
];
pub(crate) const MAX_INLINE_ROWS: usize = HEADER_ROWS + SETTING_ROWS;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SettingsView<'a> {
    pub(crate) snapshot: &'a SettingsSnapshot,
    pub(crate) category: SettingCategory,
    pub(crate) query: &'a str,
    pub(crate) selected: usize,
    pub(crate) window_start: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Layout {
    match_count: usize,
    selected: usize,
    first_item: usize,
    visible_items: usize,
    body_start_row: usize,
    row_count: usize,
}

pub(crate) fn visible_items_for_budget(view: SettingsView<'_>, budget: usize) -> usize {
    Layout::build(view, budget).visible_items.max(1)
}

pub(crate) fn settings_menu_rows(
    theme: &Theme,
    view: SettingsView<'_>,
    budget: usize,
    width: usize,
) -> Vec<Row> {
    let layout = Layout::build(view, budget);
    if width == 0 || layout.row_count == 0 {
        return Vec::new();
    }
    let mut rows = Vec::with_capacity(layout.row_count);
    if layout.body_start_row > 0 {
        rows.push(header_row(theme, view, width));
        rows.push(Row::new());
    }
    if layout.match_count == 0 {
        rows.push(Row::styled(
            &single_line_ellipsized(EMPTY, width),
            theme.dim,
        ));
        return rows;
    }
    let value_column = value_column(view, width);
    for display_index in layout.first_item..layout.first_item + layout.visible_items {
        if let Some(item) = view
            .snapshot
            .item_at(view.category, view.query, display_index)
        {
            rows.push(item_row(
                theme,
                view.snapshot,
                item,
                display_index == layout.selected,
                width,
                value_column,
            ));
        }
    }
    rows
}

pub(crate) fn settings_menu_hint_row(theme: &Theme, width: usize, ctrl_c_pending: bool) -> Row {
    if ctrl_c_pending {
        return Row::styled(CTRL_C_EXIT_HINT, theme.statusline).clipped(width);
    }
    let hint = HINTS
        .into_iter()
        .find(|hint| visible_width(hint) <= width)
        .unwrap_or(HINTS[HINTS.len() - 1]);
    Row::styled(hint, theme.dim).clipped(width)
}

impl Layout {
    fn build(view: SettingsView<'_>, max_rows: usize) -> Self {
        if max_rows == 0 {
            return Self::default();
        }
        let match_count = view.snapshot.filtered_count(view.category, view.query);
        let body_start_row = if max_rows > 2 { HEADER_ROWS } else { 0 };
        if match_count == 0 {
            return Self {
                body_start_row,
                row_count: max_rows.min(body_start_row + 1),
                ..Self::default()
            };
        }
        let selected = view.selected % match_count;
        let body_budget = max_rows - body_start_row;
        let visible_items = body_budget.min(match_count);
        let mut first_item = view.window_start.min(match_count - 1).min(selected);
        if selected >= first_item + visible_items {
            first_item = selected + 1 - visible_items;
        }
        let visible_items = visible_items.min(match_count - first_item);
        Self {
            match_count,
            selected,
            first_item,
            visible_items,
            body_start_row,
            row_count: body_start_row + visible_items,
        }
    }
}

fn header_row(theme: &Theme, view: SettingsView<'_>, width: usize) -> Row {
    let title = format!(
        "Settings {}",
        view.snapshot.filtered_count(view.category, view.query)
    );
    let tab = |row: &mut Row, category: SettingCategory, active: bool| {
        if active {
            row.push_fmt(
                format_args!("[{}]", category.label()),
                theme.selected_completion,
            );
        } else {
            row.push(category.label(), theme.dim);
        }
    };
    let mut wide = Row::styled(&title, theme.selected_completion);
    for category in SettingCategory::ALL {
        wide.push_spaces(2);
        tab(&mut wide, category, category == view.category);
    }
    if wide.width() <= width {
        return wide;
    }
    let mut compact = Row::styled(&title, theme.selected_completion);
    compact.push_spaces(2);
    tab(&mut compact, view.category, true);
    if compact.width() <= width {
        return compact;
    }
    let mut active = Row::new();
    tab(&mut active, view.category, true);
    active.clipped(width)
}

fn item_row(
    theme: &Theme,
    snapshot: &SettingsSnapshot,
    item: SettingItem<'_>,
    selected: bool,
    width: usize,
    value_column: usize,
) -> Row {
    let indent = if width <= INDENT { 0 } else { INDENT };
    let mut row = Row::new();
    row.push_spaces(indent);
    let label_paint = if selected {
        theme.selected_completion
    } else {
        theme.dim
    };
    row.push(
        &single_line_ellipsized(item.label, value_column.saturating_sub(indent)),
        label_paint,
    );
    row.push_spaces(value_column.saturating_sub(row.width()));
    let option_count = snapshot.option_count(item.id);
    if option_count == 0 {
        let room = width.saturating_sub(value_column);
        row.push(
            &single_line_ellipsized(&terminal_safe(item.value), room),
            theme.selected_completion,
        );
        return row;
    }
    for index in 0..option_count {
        if index > 0 {
            row.push_spaces(OPTION_GAP);
        }
        let Some(option) = snapshot.option_at(item.id, index) else {
            continue;
        };
        let paint = if option.eq_ignore_ascii_case(item.value) {
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
    row.clipped(width)
}

fn value_column(view: SettingsView<'_>, width: usize) -> usize {
    let indent = if width <= INDENT { 0 } else { INDENT };
    let widest = (0..)
        .map_while(|index| view.snapshot.item_at(view.category, view.query, index))
        .map(|item| visible_width(item.label))
        .max()
        .unwrap_or(0);
    (indent + widest + COLUMN_GAP).min(width)
}

#[cfg(test)]
mod tests;
