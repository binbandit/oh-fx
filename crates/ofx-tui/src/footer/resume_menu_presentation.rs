use std::borrow::Cow;

use ofx_contract::{ResumeRefusal, SessionRow, SessionScope};
use ofx_text::{prefix_by_width, visible_width};

use crate::list_window::update_edge_start;
use crate::row_text::{Row, single_line_ellipsized, single_line_middle_ellipsized, terminal_safe};
use crate::theme::Theme;

const HEADER_ROWS: usize = 1;
const ROOMY_TOP_GAP_ROWS: usize = 1;
const MAX_VISIBLE_ITEMS: usize = 20;
pub(crate) const MAX_INLINE_ROWS: usize = HEADER_ROWS + ROOMY_TOP_GAP_ROWS + MAX_VISIBLE_ITEMS;
const MINIMUM_TITLE_COLUMN_WIDTH: usize = 12;
const COLUMN_GAP_WIDTH: usize = 4;
const SEPARATOR: &str = " · ";
const FX_MARKER: &str = "fx";
pub(crate) const FALLBACK_TITLE: &str = "Untitled session";
const MS_PER_MINUTE: i64 = 60_000;
const MS_PER_HOUR: i64 = 60 * MS_PER_MINUTE;
const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadState {
    Loading,
    Ready,
    Failed,
}

pub(crate) struct SessionMenuView<'a> {
    pub(crate) scope: SessionScope,
    pub(crate) load: LoadState,
    pub(crate) rows: Vec<&'a SessionRow>,
    pub(crate) has_more: bool,
    pub(crate) loading_more: bool,
    pub(crate) selected: usize,
    pub(crate) window_start: usize,
    pub(crate) refusal: Option<ResumeRefusal>,
    pub(crate) now_ms: i64,
}

impl SessionMenuView<'_> {
    fn navigation_count(&self) -> usize {
        self.rows.len() + usize::from(self.has_more)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MenuLayout {
    session_count: usize,
    navigation_count: usize,
    selected: usize,
    visible_session_items: usize,
    first_item_row: usize,
    load_more_row: Option<usize>,
    feedback_row: Option<usize>,
    feedback_inline: bool,
    show_header: bool,
    row_count: usize,
}

impl MenuLayout {
    fn build(view: &SessionMenuView<'_>, budget: usize) -> Self {
        if budget == 0 {
            return Self::default();
        }
        let session_count = view.rows.len();
        let navigation_count = view.navigation_count();
        let selected = if navigation_count > 0 {
            view.selected % navigation_count
        } else {
            0
        };
        let base = Self {
            session_count,
            navigation_count,
            selected,
            first_item_row: HEADER_ROWS,
            show_header: true,
            ..Self::default()
        };
        let failed = view.refusal.is_some();
        if view.load != LoadState::Ready || navigation_count == 0 {
            return Self {
                show_header: budget > 1,
                row_count: budget.min(HEADER_ROWS + ROOMY_TOP_GAP_ROWS + 1),
                ..base
            };
        }
        let single = Self {
            visible_session_items: 1,
            first_item_row: 1,
            ..base
        };
        match budget {
            1 => {
                return Self {
                    visible_session_items: usize::from(session_count > 0),
                    first_item_row: 0,
                    show_header: false,
                    row_count: 1,
                    ..base
                };
            }
            2 if failed => {
                return Self {
                    feedback_inline: true,
                    row_count: 2,
                    ..single
                };
            }
            3 if failed => {
                return Self {
                    feedback_row: Some(2),
                    row_count: 3,
                    ..single
                };
            }
            4 | 5 if failed && view.has_more => {
                return Self {
                    first_item_row: 2,
                    load_more_row: Some(budget - 1),
                    feedback_row: Some(1),
                    row_count: budget,
                    ..single
                };
            }
            _ => {}
        }
        if view.has_more {
            let mut layout = Self::paged(base, budget);
            if failed {
                layout.feedback_row = Some(1);
            }
            return layout;
        }
        if budget == 2 {
            return Self {
                row_count: 2,
                ..single
            };
        }
        let first_item_row = HEADER_ROWS + ROOMY_TOP_GAP_ROWS;
        let visible = session_count
            .min((budget - first_item_row).max(1))
            .min(MAX_VISIBLE_ITEMS);
        Self {
            visible_session_items: visible,
            first_item_row,
            feedback_row: failed.then_some(1),
            row_count: first_item_row + visible,
            ..base
        }
    }

    fn paged(base: Self, budget: usize) -> Self {
        if budget == 2 {
            let load_more_selected = base.selected >= base.session_count;
            return Self {
                visible_session_items: usize::from(!load_more_selected),
                first_item_row: 1,
                load_more_row: load_more_selected.then_some(1),
                row_count: 2,
                ..base
            };
        }
        let first_item_row = HEADER_ROWS + ROOMY_TOP_GAP_ROWS;
        let available = budget.saturating_sub(first_item_row + 1);
        let visible = base.session_count.min(available).min(MAX_VISIBLE_ITEMS - 1);
        let load_more_row = first_item_row + visible;
        Self {
            visible_session_items: visible,
            first_item_row,
            load_more_row: Some(load_more_row),
            row_count: budget.min(load_more_row + 1),
            ..base
        }
    }

    fn has_load_more_selected(&self) -> bool {
        self.navigation_count > self.session_count && self.selected == self.session_count
    }

    fn window_start(&self, current: usize) -> usize {
        if self.session_count == 0 || self.visible_session_items == 0 {
            return 0;
        }
        if self.selected >= self.session_count {
            return self
                .session_count
                .saturating_sub(self.visible_session_items);
        }
        update_edge_start(
            current,
            self.session_count,
            self.selected,
            self.visible_session_items,
        )
    }
}

pub(crate) struct MenuFrame {
    pub(crate) rows: Vec<Row>,
    pub(crate) window_start: usize,
}

pub(crate) fn menu_frame(
    view: &SessionMenuView<'_>,
    theme: &Theme,
    width: usize,
    budget: usize,
) -> MenuFrame {
    let layout = MenuLayout::build(view, budget);
    let window_start = layout.window_start(view.window_start);
    let columns = Columns::measure(view);
    let rows = (0..layout.row_count)
        .map(|index| compose_row(view, &layout, window_start, &columns, theme, index, width))
        .collect();
    MenuFrame { rows, window_start }
}

fn compose_row(
    view: &SessionMenuView<'_>,
    layout: &MenuLayout,
    window_start: usize,
    columns: &Columns,
    theme: &Theme,
    index: usize,
    width: usize,
) -> Row {
    if width == 0 {
        return Row::new();
    }
    if layout.show_header && index == 0 {
        return header_row(view, theme, width);
    }
    if layout.feedback_row == Some(index)
        && let Some(refusal) = view.refusal
    {
        return refusal_row(refusal, theme, width);
    }
    if view.load != LoadState::Ready || layout.navigation_count == 0 {
        if index + 1 < layout.row_count {
            return Row::new();
        }
        return state_row(view.load, theme, width);
    }
    if layout.load_more_row == Some(index) {
        let selected = layout.has_load_more_selected();
        return load_more_row(selected, view.loading_more, theme, width);
    }
    if index < layout.first_item_row {
        return Row::new();
    }
    let offset = index - layout.first_item_row;
    if offset >= layout.visible_session_items {
        return Row::new();
    }
    let display_index = window_start + offset;
    let Some(summary) = view.rows.get(display_index) else {
        return Row::new();
    };
    let selected = display_index == layout.selected;
    if layout.feedback_inline {
        return compact_refusal_title_row(summary, selected, view.now_ms, columns, theme, width);
    }
    title_row(summary, selected, view.now_ms, columns, theme, width)
}

fn header_row(view: &SessionMenuView<'_>, theme: &Theme, width: usize) -> Row {
    let title = format!("Sessions {}", view.rows.len());
    let mut wide = Row::styled(&title, theme.selected_completion);
    for scope in [SessionScope::CurrentWorkspace, SessionScope::AllWorkspaces] {
        wide.push_spaces(2);
        push_scope_tab(&mut wide, scope, scope == view.scope, theme);
    }
    if wide.width() <= width {
        return wide;
    }
    let mut compact = Row::styled(&title, theme.selected_completion);
    compact.push_spaces(2);
    push_scope_tab(&mut compact, view.scope, true, theme);
    compact.clipped(width)
}

fn push_scope_tab(row: &mut Row, scope: SessionScope, active: bool, theme: &Theme) {
    let label = match scope {
        SessionScope::CurrentWorkspace => "Current workspace",
        SessionScope::AllWorkspaces => "All workspaces",
    };
    if active {
        row.push_fmt(format_args!("[{label}]"), theme.selected_completion);
    } else {
        row.push(label, theme.dim);
    }
}

#[derive(Debug, Default)]
struct Columns {
    title: usize,
    workspace: usize,
    age: usize,
    turns: usize,
    marks_fx: bool,
}

impl Columns {
    fn measure(view: &SessionMenuView<'_>) -> Self {
        let mut columns = Self::default();
        for summary in &view.rows {
            columns.title = columns.title.max(visible_width(&display_title(summary)));
            columns.workspace = columns
                .workspace
                .max(visible_width(&workspace_label(summary)));
            columns.age = columns
                .age
                .max(compact_age(summary.updated_at_ms, view.now_ms).len());
            columns.turns = columns.turns.max(turns_text(summary.turns).len());
            columns.marks_fx |= summary.from_fx;
        }
        columns
    }

    fn marker_width(&self) -> usize {
        if self.marks_fx {
            SEPARATOR.chars().count() + FX_MARKER.len()
        } else {
            0
        }
    }
}

fn single_line(text: &str) -> Cow<'_, str> {
    if text.contains(['\n', '\r']) {
        Cow::Owned(terminal_safe(&text.replace(['\n', '\r'], " ")).into_owned())
    } else {
        terminal_safe(text)
    }
}

fn display_title(summary: &SessionRow) -> Cow<'_, str> {
    single_line(summary.title.as_deref().unwrap_or(FALLBACK_TITLE))
}

fn workspace_label(summary: &SessionRow) -> Cow<'_, str> {
    let root = summary.workspace_root.trim_end_matches('/');
    single_line(root.rsplit_once('/').map_or(root, |(_, name)| name))
}

fn turns_text(turns: usize) -> String {
    format!("{turns} {}", if turns == 1 { "turn" } else { "turns" })
}

fn compact_age(updated_at_ms: i64, now_ms: i64) -> String {
    let delta = now_ms.saturating_sub(updated_at_ms).max(0);
    if delta < MS_PER_MINUTE {
        return "now".to_owned();
    }
    if delta < MS_PER_HOUR {
        return format!("{}m", delta / MS_PER_MINUTE);
    }
    if delta < MS_PER_DAY {
        return format!("{}h", delta / MS_PER_HOUR);
    }
    format!("{}d", delta / MS_PER_DAY)
}

fn title_row(
    summary: &SessionRow,
    selected: bool,
    now_ms: i64,
    columns: &Columns,
    theme: &Theme,
    width: usize,
) -> Row {
    let mut row = Row::new();
    let indent = if width <= 4 { 0 } else { 2 };
    row.push_spaces(indent);
    let paint = if selected {
        theme.selected_completion
    } else {
        theme.dim
    };
    let title = display_title(summary);
    let workspace = workspace_label(summary);
    let age = compact_age(summary.updated_at_ms, now_ms);
    let turns = turns_text(summary.turns);
    let workspace_col = columns.workspace.max(visible_width(&workspace));
    let age_col = columns.age.max(age.len());
    let turns_col = columns.turns.max(turns.len());
    let metadata_width = workspace_col
        + SEPARATOR.chars().count() * 2
        + age_col
        + turns_col
        + columns.marker_width();
    let content_width = width.saturating_sub(1);
    let available_title = content_width.saturating_sub(indent + COLUMN_GAP_WIDTH + metadata_width);
    let show_metadata = available_title >= MINIMUM_TITLE_COLUMN_WIDTH;
    let title_budget = if show_metadata {
        columns
            .title
            .max(visible_width(&title))
            .min(available_title)
    } else {
        width.saturating_sub(indent)
    };
    row.push(&single_line_middle_ellipsized(&title, title_budget), paint);
    if show_metadata {
        row.pad_to_column(indent + title_budget + COLUMN_GAP_WIDTH);
        row.push(&workspace, paint);
        row.push(
            &" ".repeat(workspace_col - visible_width(&workspace)),
            paint,
        );
        row.push(SEPARATOR, paint);
        row.push(&" ".repeat(age_col - age.len()), paint);
        row.push(&age, paint);
        row.push(SEPARATOR, paint);
        row.push(&turns, paint);
        if summary.from_fx {
            row.push(&" ".repeat(turns_col - turns.len()), paint);
            row.push(SEPARATOR, paint);
            row.push(FX_MARKER, paint);
        }
    }
    row
}

fn compact_refusal_title_row(
    summary: &SessionRow,
    selected: bool,
    now_ms: i64,
    columns: &Columns,
    theme: &Theme,
    width: usize,
) -> Row {
    const RETRY: &str = " · retry";
    let suffix = width.min(RETRY.chars().count());
    let mut row = title_row(summary, selected, now_ms, columns, theme, width - suffix);
    row.push(prefix_by_width(RETRY, suffix), theme.red);
    row
}

fn load_more_row(selected: bool, loading: bool, theme: &Theme, width: usize) -> Row {
    let indent = if width <= 4 { 0 } else { 2 };
    let mut row = Row::new();
    row.push_spaces(indent);
    let label = if loading {
        "↓ Loading more…"
    } else {
        "↓ Load more"
    };
    let paint = if selected {
        theme.selected_completion
    } else {
        theme.dim
    };
    row.push(&single_line_ellipsized(label, width - indent), paint);
    row
}

fn state_row(load: LoadState, theme: &Theme, width: usize) -> Row {
    let message = match load {
        LoadState::Loading => "Loading sessions…",
        LoadState::Failed => "Unable to load sessions.",
        LoadState::Ready => "No sessions found.",
    };
    let mut row = Row::new();
    if width > 4 {
        row.push_spaces(2);
    }
    row.push(prefix_by_width(message, width.saturating_sub(2)), theme.dim);
    row
}

fn refusal_row(refusal: ResumeRefusal, theme: &Theme, width: usize) -> Row {
    let indent = if width > 4 { 2 } else { 0 };
    let message = match refusal {
        ResumeRefusal::OpenElsewhere => {
            "This session is open in another oh-fx. Close it there, then press enter to retry."
        }
        ResumeRefusal::Unavailable => "Unable to resume this session.",
    };
    let mut row = Row::new();
    row.push_spaces(indent);
    row.push(&single_line_ellipsized(message, width - indent), theme.red);
    row
}

#[cfg(test)]
mod tests;
