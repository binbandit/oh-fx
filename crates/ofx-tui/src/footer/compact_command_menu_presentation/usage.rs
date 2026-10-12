use ofx_contract::{UsageCompleteness, UsageCoverage, UsageReport, UsageScope, UsageTotals};
use ofx_text::visible_width;

use super::workspace::{action_row, indent};
use crate::row_text::{Paint, Row, single_line_ellipsized};
use crate::theme::Theme;

mod models;
mod numbers;
mod overview;

use models::{ModelMode, model_column_header, model_mode, model_row};
use numbers::{compact_unsigned, money};
use overview::{OverviewMode, activity_row, overview_columns, overview_row};

const COLUMN_GAP: usize = 4;
const MAX_MODEL_ROWS: usize = 20;
const CONSTRAINED_MODEL_START: usize = 4;
const MEDIUM_WIDTH: usize = 48;
const WIDE_HEADER_WIDTH: usize = 64;
const WIDE_WIDTH: usize = 110;
const MINIMUM_NAME_WIDTH: usize = 10;
const ACTIVITY_ROWS: usize = 3;
const TITLE: &str = "Usage";
const TABS: [UsageScope; 4] = [
    UsageScope::Days30,
    UsageScope::Days7,
    UsageScope::Hours24,
    UsageScope::Session,
];
const LOADING: &str = "Loading usage";
const UNAVAILABLE: &str = "Usage unavailable · press r to retry";
const HINTS: [&str; 3] = [
    "tab scope     ↑↓ model     enter expand     r refresh     esc close",
    "tab scope  ↑↓ model  enter expand  r refresh  esc",
    "tab ↑↓  enter  r  esc",
];
const EMPTY_TOTALS: UsageTotals = UsageTotals {
    total_tokens: 0,
    input_tokens: 0,
    output_tokens: 0,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    reasoning_tokens: None,
    request_count: None,
    total_cost: 0.0,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UsageMenuView<'a> {
    pub(crate) scope: UsageScope,
    pub(crate) selected_model: usize,
    pub(crate) expanded_model: Option<usize>,
    pub(crate) model_window_start: usize,
    pub(crate) report: Option<&'a UsageReport>,
    pub(crate) refresh_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    overview_mode: OverviewMode,
    overview_start: usize,
    overview_rows: usize,
    activity_start: Option<usize>,
    models_header: usize,
    model_columns: Option<usize>,
    model_start: usize,
}

pub(crate) fn usage_desired_row_count(view: &UsageMenuView<'_>, width: usize) -> usize {
    let Some(report) = view.report else {
        return 2;
    };
    if report.totals.is_none() {
        let activity_rows = if report.session_activity.is_some() {
            ACTIVITY_ROWS
        } else {
            0
        };
        return 2 + activity_rows;
    }
    let model_rows =
        report.models.len().min(MAX_MODEL_ROWS) + usize::from(view.expanded_model.is_some());
    layout(report, width).model_start + model_rows.max(1)
}

pub(crate) fn usage_menu_rows(
    theme: &Theme,
    view: &UsageMenuView<'_>,
    visible_rows: usize,
    width: usize,
) -> Vec<Row> {
    if width == 0 {
        return vec![Row::new(); visible_rows];
    }
    (0..visible_rows)
        .map(|row_index| usage_row(theme, view, row_index, visible_rows, width))
        .collect()
}

pub(crate) fn usage_menu_hint_row(theme: &Theme, width: usize) -> Row {
    let hint = HINTS
        .into_iter()
        .find(|hint| visible_width(hint) <= width)
        .unwrap_or(HINTS[HINTS.len() - 1]);
    Row::styled(hint, theme.dim).clipped(width)
}

pub(crate) fn usage_visible_model_items(
    view: &UsageMenuView<'_>,
    visible_rows: usize,
    width: usize,
) -> usize {
    let Some(report) = view.report.filter(|report| !report.models.is_empty()) else {
        return 0;
    };
    let Some(model_start) = model_area_start(report, visible_rows, width) else {
        return 0;
    };
    let model_area_rows = visible_rows.saturating_sub(model_start);
    if model_area_rows == 0 {
        return 0;
    }
    report.models.len().min(MAX_MODEL_ROWS).min(
        model_area_rows
            .saturating_sub(usize::from(view.expanded_model.is_some()))
            .max(1),
    )
}

fn usage_row(
    theme: &Theme,
    view: &UsageMenuView<'_>,
    row_index: usize,
    visible_rows: usize,
    width: usize,
) -> Row {
    if visible_rows == 1 {
        return priority_row(theme, view, width);
    }
    if row_index == 0 {
        return header_row(theme, view.scope, width);
    }
    let Some(report) = view.report else {
        return if row_index == 1 {
            styled(waiting_text(view), width, theme.dim)
        } else {
            Row::new()
        };
    };
    let Some(totals) = report.totals else {
        if row_index == 1 {
            return status_row(theme, view, report, width);
        }
        return report
            .session_activity
            .and_then(|activity| activity_row(theme, &activity, row_index, 2, width))
            .unwrap_or_default();
    };
    let layout = layout(report, width);
    if visible_rows < layout.model_start + 1 {
        return constrained_row(theme, view, report, row_index, visible_rows, width);
    }
    if row_index == 1 {
        return status_row(theme, view, report, width);
    }
    let overview = layout.overview_start..layout.overview_start + layout.overview_rows;
    if overview.contains(&row_index) {
        return overview_row(
            theme,
            &totals,
            layout.overview_mode,
            row_index - layout.overview_start,
            width,
        );
    }
    let activity = layout.activity_start.zip(report.session_activity);
    if let Some(row) = activity
        .and_then(|(start, activity)| activity_row(theme, &activity, row_index, start, width))
    {
        return row;
    }
    if row_index == layout.models_header {
        return models_header(theme, report.models.len(), width);
    }
    if layout.model_columns == Some(row_index) {
        return model_column_header(theme, report, width);
    }
    if row_index < layout.model_start {
        return Row::new();
    }
    if report.models.is_empty() {
        return if row_index == layout.model_start {
            styled("  No model usage in this scope.", width, theme.dim)
        } else {
            Row::new()
        };
    }
    model_row(
        theme,
        view,
        report,
        row_index - layout.model_start,
        visible_rows - layout.model_start,
        width,
    )
}

fn constrained_row(
    theme: &Theme,
    view: &UsageMenuView<'_>,
    report: &UsageReport,
    row_index: usize,
    visible_rows: usize,
    width: usize,
) -> Row {
    let model_start = constrained_model_start(visible_rows);
    match row_index {
        1 if row_index < model_start => status_row(theme, view, report, width),
        2 if row_index < model_start => compact_summary_row(theme, report, width),
        3 if row_index < model_start => models_header(theme, report.models.len(), width),
        index if index < model_start => Row::new(),
        _ if report.models.is_empty() => status_row(theme, view, report, width),
        index => model_row(
            theme,
            view,
            report,
            index - model_start,
            visible_rows - model_start,
            width,
        ),
    }
}

fn layout(report: &UsageReport, width: usize) -> Layout {
    let totals = report.totals.unwrap_or(EMPTY_TOTALS);
    let overview_mode = if width >= WIDE_WIDTH && overview_columns(&totals, width).is_some() {
        OverviewMode::Wide
    } else if width >= MEDIUM_WIDTH {
        OverviewMode::Medium
    } else {
        OverviewMode::Narrow
    };
    let narrow = overview_mode == OverviewMode::Narrow;
    let overview_start = if narrow { 2 } else { 3 };
    let overview_rows = if narrow { 2 } else { 3 };
    let mut cursor = overview_start + overview_rows;
    let mut activity_start = None;
    if report.session_activity.is_some() {
        cursor += usize::from(!narrow);
        activity_start = Some(cursor);
        cursor += ACTIVITY_ROWS;
    }
    cursor += usize::from(!narrow);
    let models_header = cursor;
    cursor += 1;
    let mut model_columns = None;
    if model_mode(report, width) == ModelMode::Columns {
        model_columns = Some(cursor);
        cursor += 1;
    }
    Layout {
        overview_mode,
        overview_start,
        overview_rows,
        activity_start,
        models_header,
        model_columns,
        model_start: cursor,
    }
}

fn priority_row(theme: &Theme, view: &UsageMenuView<'_>, width: usize) -> Row {
    let Some(report) = view.report else {
        return styled(waiting_text(view), width, theme.dim);
    };
    if report.models.is_empty() {
        return status_row(theme, view, report, width);
    }
    model_row(theme, view, report, 0, 1, width)
}

fn waiting_text(view: &UsageMenuView<'_>) -> &'static str {
    if view.refresh_failed {
        UNAVAILABLE
    } else {
        LOADING
    }
}

fn header_row(theme: &Theme, active: UsageScope, width: usize) -> Row {
    let mut wide = Row::styled(TITLE, theme.selected_completion);
    for scope in TABS {
        wide.push_spaces(2);
        push_tab(&mut wide, theme, scope, scope == active);
    }
    if width >= WIDE_HEADER_WIDTH && wide.width() <= width {
        return wide;
    }
    let mut compact = Row::styled(TITLE, theme.selected_completion);
    compact.push_spaces(2);
    push_tab(&mut compact, theme, active, true);
    compact.clipped(width)
}

fn push_tab(row: &mut Row, theme: &Theme, scope: UsageScope, active: bool) {
    if active {
        row.push(&format!("[{}]", scope.label()), theme.selected_completion);
    } else {
        row.push(scope.label(), theme.dim);
    }
}

fn status_row(theme: &Theme, view: &UsageMenuView<'_>, report: &UsageReport, width: usize) -> Row {
    let status = if view.refresh_failed {
        "Refresh failed · showing previous data"
    } else if report.coverage == UsageCoverage::NotStarted {
        "Tracking has not started"
    } else if report.coverage == UsageCoverage::Partial
        || report.completeness != UsageCompleteness::Complete
    {
        if width < MEDIUM_WIDTH {
            "Partial data"
        } else {
            "Partial data · some usage may be missing"
        }
    } else {
        "Local oh-fx activity"
    };
    styled(status, width, theme.dim)
}

fn compact_summary_row(theme: &Theme, report: &UsageReport, width: usize) -> Row {
    let Some(totals) = report.totals else {
        return styled("Usage unavailable", width, theme.dim);
    };
    let summary = format!(
        "{} tokens · {}",
        compact_unsigned(totals.total_tokens),
        money(totals.total_cost)
    );
    styled(&summary, width, theme.dim)
}

fn models_header(theme: &Theme, model_count: usize, width: usize) -> Row {
    let text = if width < MEDIUM_WIDTH {
        "Models".to_owned()
    } else {
        format!("Models {model_count}")
    };
    styled(&text, width, theme.system_notice_label)
}

fn model_area_start(report: &UsageReport, visible_rows: usize, width: usize) -> Option<usize> {
    report.totals?;
    if visible_rows == 1 {
        return Some(0);
    }
    let natural = layout(report, width).model_start;
    if visible_rows < natural + 1 {
        return Some(constrained_model_start(visible_rows));
    }
    Some(natural)
}

fn constrained_model_start(visible_rows: usize) -> usize {
    if visible_rows <= CONSTRAINED_MODEL_START {
        visible_rows.saturating_sub(1)
    } else {
        CONSTRAINED_MODEL_START
    }
}

fn styled(text: &str, width: usize, paint: Paint) -> Row {
    Row::styled(&single_line_ellipsized(text, width), paint)
}

#[cfg(test)]
mod tests;
