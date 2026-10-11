use ofx_contract::{ModelUsage, UsageReport, UsageTotals};
use ofx_text::{prefix_by_width, visible_width};

use super::numbers::{compact_unsigned, grouped_unsigned, money, usage_share};
use super::{
    COLUMN_GAP, MAX_MODEL_ROWS, MEDIUM_WIDTH, MINIMUM_NAME_WIDTH, UsageMenuView, WIDE_WIDTH,
    action_row, indent, styled,
};
use crate::row_text::{Row, single_line_ellipsized, terminal_safe};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModelMode {
    Columns,
    Facts,
    Compact,
    NameOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ModelColumns {
    token: usize,
    share: usize,
    spend: usize,
}

pub(super) fn model_row(
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

pub(super) fn model_column_header(theme: &Theme, report: &UsageReport, width: usize) -> Row {
    model_columns(report, width).map_or_else(Row::new, |columns| {
        columns_row(
            theme,
            ["Model", "Tokens", "Share", "Spend"],
            false,
            columns,
            width,
        )
    })
}

pub(super) fn model_summary_row(
    theme: &Theme,
    report: &UsageReport,
    model: &ModelUsage,
    selected: bool,
    width: usize,
) -> Row {
    let mode = model_mode(report, width);
    if mode == ModelMode::Columns
        && let Some(columns) = model_columns(report, width)
    {
        let name = terminal_safe(&model.model);
        let tokens = compact_unsigned(model.totals.total_tokens);
        let share = usage_share(model.totals.total_tokens, total_tokens(report));
        let spend = money(model.totals.total_cost);
        return columns_row(
            theme,
            [&name, &tokens, &share, &spend],
            selected,
            columns,
            width,
        );
    }
    let include_share = mode == ModelMode::Facts;
    let info = match mode {
        ModelMode::NameOnly => String::new(),
        ModelMode::Columns | ModelMode::Facts | ModelMode::Compact => {
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

pub(super) fn columns_row(
    theme: &Theme,
    [name, tokens, share, spend]: [&str; 4],
    selected: bool,
    columns: ModelColumns,
    width: usize,
) -> Row {
    let paint = if selected {
        theme.system_notice_label
    } else {
        theme.dim
    };
    let mut row = Row::styled(
        prefix_by_width(if selected { "❯ " } else { "  " }, width),
        paint,
    );
    let used = row.width();
    row.push(
        &single_line_ellipsized(name, columns.token.saturating_sub(used + 1)),
        paint,
    );
    row.pad_to_column(columns.token);
    row.push(
        &single_line_ellipsized(tokens, columns.share.saturating_sub(columns.token + 1)),
        theme.dim,
    );
    row.pad_to_column(columns.share);
    row.push(
        &single_line_ellipsized(share, columns.spend.saturating_sub(columns.share + 1)),
        theme.dim,
    );
    row.pad_to_column(columns.spend);
    row.push(
        &single_line_ellipsized(spend, width.saturating_sub(columns.spend)),
        theme.dim,
    );
    row
}

pub(super) fn model_columns(report: &UsageReport, width: usize) -> Option<ModelColumns> {
    let total = total_tokens(report);
    let widest = |header: &str, cell: &dyn Fn(&ModelUsage) -> String| {
        report
            .models
            .iter()
            .map(|model| visible_width(&cell(model)))
            .fold(visible_width(header), usize::max)
    };
    let longest_model = widest("Model", &|model| terminal_safe(&model.model).into_owned());
    let token_width = widest("Tokens", &|model| {
        compact_unsigned(model.totals.total_tokens)
    });
    let share_width = widest("Share", &|model| {
        usage_share(model.totals.total_tokens, total)
    });
    let spend_width = widest("Spend", &|model| money(model.totals.total_cost));
    let fixed = COLUMN_GAP * 3 + token_width + share_width + spend_width;
    if width < indent(width) + MINIMUM_NAME_WIDTH + fixed {
        return None;
    }
    let name_width = longest_model.min(width - indent(width) - fixed);
    let token = indent(width) + name_width + COLUMN_GAP;
    let share = token + token_width + COLUMN_GAP;
    Some(ModelColumns {
        token,
        share,
        spend: share + share_width + COLUMN_GAP,
    })
}

pub(super) fn model_mode(report: &UsageReport, width: usize) -> ModelMode {
    if width >= WIDE_WIDTH && model_columns(report, width).is_some() {
        ModelMode::Columns
    } else if width >= MEDIUM_WIDTH && info_column(report, width, true).is_some() {
        ModelMode::Facts
    } else if info_column(report, width, false).is_some() {
        ModelMode::Compact
    } else {
        ModelMode::NameOnly
    }
}

pub(super) fn info_column(
    report: &UsageReport,
    width: usize,
    include_share: bool,
) -> Option<usize> {
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

pub(super) fn model_facts(model: &ModelUsage, total_tokens: u64, include_share: bool) -> String {
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

pub(super) fn model_detail(totals: &UsageTotals) -> String {
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

pub(super) fn total_tokens(report: &UsageReport) -> u64 {
    report.totals.map_or(0, |totals| totals.total_tokens)
}
