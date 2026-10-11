use ofx_contract::{SessionActivity, UsageTotals};
use ofx_text::visible_width;

use super::numbers::{format_duration, money, request_label, token_label};
use super::{COLUMN_GAP, indent, styled};
use crate::row_text::{Row, single_line_ellipsized};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OverviewMode {
    Wide,
    Medium,
    Narrow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OverviewColumns {
    second: usize,
    third: usize,
}

pub(super) fn activity_row(
    theme: &Theme,
    activity: &SessionActivity,
    row_index: usize,
    activity_start: usize,
    width: usize,
) -> Option<Row> {
    let duration = |complete: bool, duration_ms: u64| {
        if complete {
            format_duration(duration_ms)
        } else {
            "Unavailable".to_owned()
        }
    };
    match row_index.checked_sub(activity_start)? {
        0 => Some(styled("Session activity", width, theme.system_notice_label)),
        1 => Some(styled(
            &format!(
                "API {} · Wall {}",
                duration(activity.api_duration_complete, activity.api_duration_ms),
                duration(activity.wall_duration_complete, activity.wall_duration_ms)
            ),
            width,
            theme.dim,
        )),
        2 => {
            let text = if activity.code_complete {
                format!(
                    "Code +{} · -{}",
                    activity.lines_added, activity.lines_removed
                )
            } else {
                "Code activity unavailable".to_owned()
            };
            Some(styled(&text, width, theme.dim))
        }
        _ => None,
    }
}

pub(super) fn overview_row(
    theme: &Theme,
    totals: &UsageTotals,
    mode: OverviewMode,
    display_row: usize,
    width: usize,
) -> Row {
    let cells = overview_cells(totals, mode, display_row);
    if mode == OverviewMode::Wide
        && let Some(columns) = overview_columns(totals, width)
    {
        return metric_columns(theme, &cells, columns, width);
    }
    let text = cells
        .iter()
        .filter(|cell| !cell.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" · ");
    styled(&text, width, theme.dim)
}

pub(super) fn overview_cells(totals: &UsageTotals, mode: OverviewMode, row: usize) -> [String; 3] {
    if mode == OverviewMode::Narrow {
        return match row {
            0 => [
                token_label(totals.total_tokens, "tokens"),
                money(totals.total_cost),
                String::new(),
            ],
            _ => [
                request_label(totals.request_count),
                String::new(),
                String::new(),
            ],
        };
    }
    match row {
        0 => [
            token_label(totals.total_tokens, "tokens"),
            format!("{} spent", money(totals.total_cost)),
            request_label(totals.request_count),
        ],
        1 => [
            token_label(totals.input_tokens, "input"),
            token_label(totals.output_tokens, "output"),
            totals.reasoning_tokens.map_or_else(
                || "Reasoning unavailable".to_owned(),
                |reasoning| token_label(reasoning, "reasoning"),
            ),
        ],
        _ => [
            token_label(totals.cache_read_tokens, "cache read"),
            token_label(totals.cache_write_tokens, "cache write"),
            String::new(),
        ],
    }
}

pub(super) fn overview_columns(totals: &UsageTotals, width: usize) -> Option<OverviewColumns> {
    let mut widths = [0; 3];
    for row in 0..3 {
        let cells = overview_cells(totals, OverviewMode::Wide, row);
        for (widest, cell) in widths.iter_mut().zip(&cells) {
            *widest = (*widest).max(visible_width(cell));
        }
    }
    let second = indent(width) + widths[0] + COLUMN_GAP;
    let third = second + widths[1] + COLUMN_GAP;
    (third + widths[2] <= width).then_some(OverviewColumns { second, third })
}

pub(super) fn metric_columns(
    theme: &Theme,
    cells: &[String; 3],
    columns: OverviewColumns,
    width: usize,
) -> Row {
    let mut row = Row::new();
    if width > 2 {
        row.push_spaces(2);
    }
    row.push(
        &single_line_ellipsized(&cells[0], columns.second.saturating_sub(3)),
        theme.dim,
    );
    row.pad_to_column(columns.second);
    row.push(
        &single_line_ellipsized(&cells[1], columns.third.saturating_sub(columns.second + 1)),
        theme.dim,
    );
    if !cells[2].is_empty() {
        row.pad_to_column(columns.third);
        row.push(
            &single_line_ellipsized(&cells[2], width.saturating_sub(columns.third)),
            theme.dim,
        );
    }
    row
}
