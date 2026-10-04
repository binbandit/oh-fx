use ofx_contract::{SettingCategory, SettingId, SettingItem, SettingsSnapshot};
use ofx_text::visible_width;

use crate::row_text::{Row, single_line_ellipsized, terminal_safe};
use crate::shell::model_menu::{CatalogLoad, ModelMenu};
use crate::theme::Theme;

const HEADER_ROWS: usize = 2;
const SETTING_ROWS: usize = 10;
const INLINE_MODEL_ROWS: usize = 6;
const MODEL_STATE_INDENT: usize = 4;
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
    pub(crate) models: Option<InlineModels<'a>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InlineModels<'a> {
    pub(crate) menu: &'a ModelMenu,
    pub(crate) catalog: &'a CatalogLoad,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Layout {
    match_count: usize,
    selected: usize,
    first_item: usize,
    visible_items: usize,
    body_start_row: usize,
    row_count: usize,
    model_rows: usize,
    model_only: bool,
}

#[derive(Debug, Clone, Copy)]
struct ModelWindow<'a> {
    matches: &'a [usize],
    visible_rows: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct Measurement {
    visible_items: usize,
    rows: usize,
}

enum BodyRow<'a> {
    None,
    Item {
        item: SettingItem<'a>,
        selected: bool,
    },
    Model(usize),
}

pub(crate) fn visible_items_for_budget(view: SettingsView<'_>, budget: usize) -> usize {
    Layout::build(view, budget).visible_items.max(1)
}

pub(crate) fn visible_model_items_for_budget(view: SettingsView<'_>, budget: usize) -> usize {
    Layout::build(view, budget).visible_model_rows(view)
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
        rows.push(styled_text(EMPTY, width, theme, 0));
        return rows;
    }
    let value_column = value_column(view, width);
    let matches = view
        .models
        .map(|models| models.menu.matches(models.catalog.models()))
        .unwrap_or_default();
    let window = ModelWindow {
        matches: &matches,
        visible_rows: layout.visible_model_rows(view),
    };
    for target in 0..layout.row_count - layout.body_start_row {
        rows.push(match layout.body_row_at(view, target) {
            BodyRow::None => Row::new(),
            BodyRow::Item { item, selected } => {
                item_row(theme, view.snapshot, item, selected, width, value_column)
            }
            BodyRow::Model(offset) => view.models.map_or_else(Row::new, |models| {
                model_row(theme, models, window, offset, width, value_column)
            }),
        });
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
        let model_rows = inline_model_row_count(view);
        let mut first_item = if view.models.is_some() {
            selected
        } else {
            view.window_start.min(match_count - 1)
        };
        first_item = first_item.min(selected);
        let mut body = measure_body(view, model_rows, first_item, body_budget);
        while selected >= first_item + body.visible_items && first_item < selected {
            first_item += 1;
            body = measure_body(view, model_rows, first_item, body_budget);
        }
        if body.visible_items == 0 {
            first_item = selected;
            body = measure_body(view, model_rows, first_item, body_budget);
        }
        Self {
            match_count,
            selected,
            first_item,
            visible_items: body.visible_items,
            body_start_row,
            row_count: body_start_row + body.rows,
            model_rows,
            model_only: view.models.is_some() && body_budget == 1,
        }
    }

    fn visible_model_rows(&self, view: SettingsView<'_>) -> usize {
        (0..self.row_count - self.body_start_row)
            .filter(|target| matches!(self.body_row_at(view, *target), BodyRow::Model(_)))
            .count()
    }

    fn body_row_at<'a>(&self, view: SettingsView<'a>, target: usize) -> BodyRow<'a> {
        if self.model_only {
            return if target == 0 {
                BodyRow::Model(0)
            } else {
                BodyRow::None
            };
        }
        let mut row = 0;
        for display_index in self.first_item..self.first_item + self.visible_items {
            let Some(item) = view
                .snapshot
                .item_at(view.category, view.query, display_index)
            else {
                return BodyRow::None;
            };
            if row == target {
                return BodyRow::Item {
                    item,
                    selected: display_index == self.selected,
                };
            }
            row += 1;
            if item.id == SettingId::Model && view.models.is_some() {
                if target < row + self.model_rows {
                    return BodyRow::Model(target - row);
                }
                row += self.model_rows;
            }
        }
        BodyRow::None
    }
}

fn inline_model_row_count(view: SettingsView<'_>) -> usize {
    let Some(models) = view.models else {
        return 0;
    };
    let ready = matches!(models.catalog, CatalogLoad::Listed { .. });
    let count = models.menu.matches(models.catalog.models()).len();
    if !ready || count == 0 {
        1
    } else {
        count.min(INLINE_MODEL_ROWS)
    }
}

fn measure_body(
    view: SettingsView<'_>,
    model_rows: usize,
    first_item: usize,
    row_budget: usize,
) -> Measurement {
    let mut result = Measurement::default();
    let mut display_index = first_item;
    while let Some(item) = view
        .snapshot
        .item_at(view.category, view.query, display_index)
    {
        if result.rows + 1 > row_budget {
            break;
        }
        let inline = if item.id == SettingId::Model {
            model_rows.min(row_budget - result.rows - 1)
        } else {
            0
        };
        result.rows += 1 + inline;
        result.visible_items += 1;
        display_index += 1;
    }
    if view.models.is_some() && row_budget == 1 && result.visible_items > 0 {
        result.rows = 1;
    }
    result
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
    row.pad_to_column(value_column);
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

fn model_row(
    theme: &Theme,
    models: InlineModels<'_>,
    window: ModelWindow<'_>,
    visible_offset: usize,
    width: usize,
    value_column: usize,
) -> Row {
    let matches = window.matches;
    let ready = matches!(models.catalog, CatalogLoad::Listed { .. });
    if !ready || matches.is_empty() {
        let label = match models.catalog {
            CatalogLoad::Idle | CatalogLoad::Loading => "Loading models…",
            CatalogLoad::Listed { .. } => "No models found",
            CatalogLoad::Failed(_) => "Models unavailable",
        };
        return styled_text(label, width, theme, MODEL_STATE_INDENT);
    }
    let match_count = matches.len();
    let selected = models.menu.selected % match_count;
    let visible_count = match_count.min(window.visible_rows.max(1));
    let mut window_start = models.menu.window_start;
    if selected < window_start {
        window_start = selected;
    }
    if selected >= window_start + visible_count {
        window_start = selected + 1 - visible_count;
    }
    let display_index = window_start.min(match_count - visible_count) + visible_offset;
    let Some(option) = matches
        .get(display_index)
        .and_then(|index| models.catalog.models().get(*index))
    else {
        return Row::new();
    };
    let paint = if display_index == selected {
        theme.selected_completion
    } else {
        theme.dim
    };
    let mut row = Row::new();
    row.push_spaces(value_column);
    row.push(
        &single_line_ellipsized(
            &terminal_safe(&option.id),
            width.saturating_sub(value_column),
        ),
        paint,
    );
    row
}

fn styled_text(text: &str, width: usize, theme: &Theme, indent: usize) -> Row {
    let mut row = Row::new();
    row.push_spaces(indent.min(width));
    row.push(
        &single_line_ellipsized(text, width.saturating_sub(indent)),
        theme.dim,
    );
    row
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
