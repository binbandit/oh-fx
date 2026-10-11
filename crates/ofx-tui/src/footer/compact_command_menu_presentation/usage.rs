use ofx_contract::{
    ModelUsage, UsageCompleteness, UsageCoverage, UsageReport, UsageScope, UsageTotals,
};
use ofx_text::visible_width;

use super::workspace::{action_row, indent};
use crate::row_text::{Paint, Row, single_line_ellipsized, terminal_safe};
use crate::theme::Theme;

const COLUMN_GAP: usize = 4;
const MAX_MODEL_ROWS: usize = 20;
const CONSTRAINED_MODEL_START: usize = 4;
const MEDIUM_WIDTH: usize = 48;
const WIDE_HEADER_WIDTH: usize = 64;
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
const TWO_TO_THE_32: f64 = 4_294_967_296.0;

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
enum ModelMode {
    Facts,
    Compact,
    NameOnly,
}

pub(crate) fn usage_desired_row_count(view: &UsageMenuView<'_>) -> usize {
    let Some(report) = view.report.filter(|report| report.totals.is_some()) else {
        return 2;
    };
    let model_rows =
        report.models.len().min(MAX_MODEL_ROWS) + usize::from(view.expanded_model.is_some());
    CONSTRAINED_MODEL_START + model_rows.max(1)
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

pub(crate) fn usage_visible_model_items(view: &UsageMenuView<'_>, visible_rows: usize) -> usize {
    let Some(report) = view.report.filter(|report| !report.models.is_empty()) else {
        return 0;
    };
    let Some(model_start) = model_area_start(report, visible_rows) else {
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
    if report.totals.is_none() {
        return if row_index == 1 {
            status_row(theme, view, report, width)
        } else {
            Row::new()
        };
    }
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

fn model_row(
    theme: &Theme,
    view: &UsageMenuView<'_>,
    report: &UsageReport,
    display_row: usize,
    visible_rows: usize,
    width: usize,
) -> Row {
    let count = report.models.len();
    let selected = view.selected_model.min(count.saturating_sub(1));
    let visible_models = count.min(MAX_MODEL_ROWS).min(
        visible_rows
            .saturating_sub(usize::from(view.expanded_model.is_some()))
            .max(1),
    );
    let max_start = count.saturating_sub(visible_models);
    let selection_start = selected.saturating_sub(visible_models.saturating_sub(1));
    let start = view
        .model_window_start
        .min(selected)
        .max(selection_start)
        .min(max_start);
    let mut logical_row = 0;
    for (index, model) in report.models.iter().enumerate().skip(start) {
        if logical_row == display_row {
            return model_summary_row(theme, report, model, index == selected, width);
        }
        logical_row += 1;
        if view.expanded_model == Some(index) {
            if logical_row == display_row {
                return styled(&model_detail(&model.totals), width, theme.dim);
            }
            logical_row += 1;
        }
        if logical_row > display_row {
            break;
        }
    }
    Row::new()
}

fn model_summary_row(
    theme: &Theme,
    report: &UsageReport,
    model: &ModelUsage,
    selected: bool,
    width: usize,
) -> Row {
    let mode = model_mode(report, width);
    let include_share = mode == ModelMode::Facts;
    let info = match mode {
        ModelMode::NameOnly => String::new(),
        ModelMode::Facts | ModelMode::Compact => {
            model_facts(model, total_tokens(report), include_share)
        }
    };
    action_row(
        theme,
        &terminal_safe(&model.model),
        &info,
        selected,
        info_column(report, width, include_share),
        width,
    )
}

fn model_mode(report: &UsageReport, width: usize) -> ModelMode {
    if width >= MEDIUM_WIDTH && info_column(report, width, true).is_some() {
        ModelMode::Facts
    } else if info_column(report, width, false).is_some() {
        ModelMode::Compact
    } else {
        ModelMode::NameOnly
    }
}

fn info_column(report: &UsageReport, width: usize, include_share: bool) -> Option<usize> {
    let total = total_tokens(report);
    let longest_model = report
        .models
        .iter()
        .map(|model| visible_width(&terminal_safe(&model.model)))
        .max()
        .unwrap_or(0);
    let widest_info = report
        .models
        .iter()
        .map(|model| visible_width(&model_facts(model, total, include_share)))
        .max()
        .unwrap_or(0);
    if widest_info == 0 || width < indent(width) + 8 + COLUMN_GAP + widest_info {
        return None;
    }
    Some((indent(width) + longest_model + COLUMN_GAP).min(width - widest_info))
}

fn model_facts(model: &ModelUsage, total_tokens: u64, include_share: bool) -> String {
    let tokens = compact_unsigned(model.totals.total_tokens);
    let spend = money(model.totals.total_cost);
    if include_share {
        format!(
            "{tokens} · {} · {spend}",
            usage_share(model.totals.total_tokens, total_tokens)
        )
    } else {
        format!("{tokens} · {spend}")
    }
}

fn model_detail(totals: &UsageTotals) -> String {
    format!(
        "Input {} · Output {} · Cache {}/{} · Reasoning {} · Requests {} · {}",
        compact_unsigned(totals.input_tokens),
        compact_unsigned(totals.output_tokens),
        compact_unsigned(totals.cache_read_tokens),
        compact_unsigned(totals.cache_write_tokens),
        totals
            .reasoning_tokens
            .map_or_else(|| "n/a".to_owned(), compact_unsigned),
        totals
            .request_count
            .map_or_else(|| "n/a".to_owned(), grouped_unsigned),
        money(totals.total_cost)
    )
}

fn total_tokens(report: &UsageReport) -> u64 {
    report.totals.map_or(0, |totals| totals.total_tokens)
}

fn model_area_start(report: &UsageReport, visible_rows: usize) -> Option<usize> {
    report.totals?;
    if visible_rows == 1 {
        return Some(0);
    }
    Some(constrained_model_start(visible_rows))
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

fn compact_unsigned(value: u64) -> String {
    for (unit, suffix) in [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "K")] {
        if value < unit {
            continue;
        }
        if value.is_multiple_of(unit) {
            return format!("{}{suffix}", value / unit);
        }
        return format!("{:.1}{suffix}", float(value) / float(unit));
    }
    value.to_string()
}

fn grouped_unsigned(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn money(value: f64) -> String {
    format!("${value:.2}")
}

fn usage_share(tokens: u64, total_tokens: u64) -> String {
    let share = if total_tokens == 0 {
        0.0
    } else {
        float(tokens) * 100.0 / float(total_tokens)
    };
    format!("{share:.1}%")
}

fn float(value: u64) -> f64 {
    let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    f64::from(high) * TWO_TO_THE_32 + f64::from(low)
}

#[cfg(test)]
mod tests;
