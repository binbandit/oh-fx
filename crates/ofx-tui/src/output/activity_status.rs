use std::fmt;

use ofx_text::{
    StreamingEstimator, display_unit_at, status_prefix_end, suffix_by_width, trim_break_whitespace,
    visible_width, wrap_cut_ignoring_ansi,
};

use crate::row_text::{Paint, Row};
use crate::theme::Theme;

pub(crate) const ACTIVITY_BLINK_HALF_PERIOD_MS: i64 = 500;
const MAX_STATIC_STATUS_ROWS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnPhase {
    Thinking,
    Generating,
    Running,
}

impl TurnPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Thinking => "Thinking",
            Self::Generating => "Generating",
            Self::Running => "Running",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TokenProgress {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TurnTokens {
    settled: TokenProgress,
    active: StreamingEstimator,
    active_reasoning: StreamingEstimator,
}

impl TurnTokens {
    pub(crate) fn for_prompt(prompt: &str) -> Self {
        let mut estimator = StreamingEstimator::default();
        estimator.consume(prompt);
        Self {
            settled: TokenProgress {
                input_tokens: estimator.estimate(),
                output_tokens: 0,
            },
            ..Self::default()
        }
    }

    pub(crate) fn consume_content(&mut self, text: &str) {
        self.active.consume(text);
    }

    pub(crate) fn consume_reasoning(&mut self, text: &str) {
        self.active_reasoning.consume(text);
    }

    pub(crate) fn settle(&mut self, reported_output_tokens: Option<u64>) {
        let output = reported_output_tokens.unwrap_or_else(|| self.active_estimate());
        self.settled.output_tokens = self.settled.output_tokens.saturating_add(output);
        self.active = StreamingEstimator::default();
        self.active_reasoning = StreamingEstimator::default();
    }

    pub(crate) fn progress(&self) -> TokenProgress {
        TokenProgress {
            input_tokens: self.settled.input_tokens,
            output_tokens: self
                .settled
                .output_tokens
                .saturating_add(self.active_estimate()),
        }
    }

    fn active_estimate(&self) -> u64 {
        self.active
            .estimate()
            .saturating_add(self.active_reasoning.estimate())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactTokens(pub(crate) u64);

impl fmt::Display for CompactTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tokens = self.0;
        if tokens < 1000 {
            return write!(formatter, "{tokens}");
        }
        let whole = tokens / 1000;
        let tenths = (tokens % 1000) / 100;
        if whole < 10 && tenths > 0 {
            write!(formatter, "{whole}.{tenths}k")
        } else {
            write!(formatter, "{whole}k")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProgressSuffix(pub(crate) TokenProgress);

impl fmt::Display for ProgressSuffix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let progress = self.0;
        if progress.input_tokens == 0 && progress.output_tokens == 0 {
            return Ok(());
        }
        write!(
            formatter,
            " (↑{} ↓{})",
            CompactTokens(progress.input_tokens),
            CompactTokens(progress.output_tokens)
        )
    }
}

struct Elapsed(i64);

impl fmt::Display for Elapsed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.rem_euclid(60);
        let total_minutes = self.0.div_euclid(60);
        let mins = total_minutes.rem_euclid(60);
        let hours = total_minutes.div_euclid(60);
        if hours > 0 {
            write!(formatter, "{hours}h{mins}m{secs}s")
        } else if mins > 0 {
            write!(formatter, "{mins}m{secs}s")
        } else {
            write!(formatter, "{secs}s")
        }
    }
}

pub(crate) fn activity_phase(turn_started_ms: i64, now_ms: i64) -> i64 {
    (now_ms - turn_started_ms).max(0) / ACTIVITY_BLINK_HALF_PERIOD_MS
}

fn activity_blink_visible(turn_started_ms: i64, now_ms: i64) -> bool {
    now_ms < turn_started_ms || activity_phase(turn_started_ms, now_ms) % 2 == 0
}

pub(crate) fn turn_activity_row(
    theme: &Theme,
    phase: TurnPhase,
    turn_started_ms: i64,
    now_ms: i64,
    progress: TokenProgress,
    max_width: usize,
) -> Row {
    activity_row(
        theme,
        phase.label(),
        turn_started_ms,
        now_ms,
        progress,
        max_width,
    )
}

pub(crate) fn activity_row(
    theme: &Theme,
    label: &str,
    turn_started_ms: i64,
    now_ms: i64,
    progress: TokenProgress,
    max_width: usize,
) -> Row {
    let paint = theme.permission_auto;
    let mut row = Row::new();
    let marker = if activity_blink_visible(turn_started_ms, now_ms) {
        "•"
    } else {
        " "
    };
    row.push(marker, paint);
    row.push(" ", paint);
    row.push(label, paint);
    if now_ms >= turn_started_ms {
        row.push_fmt(
            format_args!(" ({})", Elapsed((now_ms - turn_started_ms) / 1000)),
            paint,
        );
    }
    row.push_fmt(format_args!("{}", ProgressSuffix(progress)), theme.dim);
    clip_with_ellipsis(row, max_width)
}

pub(crate) fn clip_with_ellipsis(row: Row, max_width: usize) -> Row {
    if row.width() <= max_width {
        return row;
    }
    match max_width {
        0 => Row::new(),
        1 => Row::plain("."),
        2 => Row::plain(".."),
        _ => {
            let mut clipped = row.clipped(max_width - 3);
            clipped.push("...", Paint::PLAIN);
            clipped
        }
    }
}

fn omission_marker(cols: usize) -> &'static str {
    match cols {
        0 => "",
        1 => ".",
        2 => "..",
        _ => "...",
    }
}

pub(crate) fn static_status_rows(label: &str, paint: Paint, cols: usize) -> Vec<Row> {
    let row = label.split(['\n', '\r']).next().unwrap_or_default();
    if visible_width(row) <= cols {
        return vec![Row::styled(row, paint)];
    }
    let prefix = &row[..status_prefix_end(row)];
    let prefix_width = visible_width(prefix);
    let continuation_indent = prefix_width.max(1);
    let mut remaining = &row[prefix.len()..];
    let mut rows = Vec::new();
    while !remaining.is_empty() && rows.len() < MAX_STATIC_STATUS_ROWS {
        let first = rows.is_empty();
        let mut line = Row::new();
        let indent = if first {
            line.push(prefix, paint);
            prefix_width
        } else {
            line.push_spaces(continuation_indent);
            continuation_indent
        };
        let available = cols.saturating_sub(indent).max(1);
        if rows.len() + 1 == MAX_STATIC_STATUS_ROWS && visible_width(remaining) > available {
            let marker = omission_marker(available);
            line.push(marker, paint);
            line.push(suffix_by_width(remaining, available - marker.len()), paint);
            remaining = "";
        } else {
            let mut chunk = wrap_cut_ignoring_ansi(remaining, available);
            if chunk.is_empty() {
                chunk = &remaining[..display_unit_at(remaining, 0).byte_len.max(1)];
            }
            line.push(chunk, paint);
            remaining = trim_break_whitespace(&remaining[chunk.len()..]);
        }
        rows.push(line);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_use_the_compact_upstream_format() {
        let compact = |tokens| CompactTokens(tokens).to_string();
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1000), "1k");
        assert_eq!(compact(1700), "1.7k");
        assert_eq!(compact(9950), "9.9k");
        assert_eq!(compact(12_000), "12k");
        assert_eq!(compact(50_400), "50k");
    }

    fn label(phase: TurnPhase, started_ms: i64, now_ms: i64, progress: TokenProgress) -> String {
        let theme = Theme::builtin(false, false, true);
        turn_activity_row(&theme, phase, started_ms, now_ms, progress, 80).text()
    }

    #[test]
    fn turn_labels_show_phase_elapsed_and_progress() {
        let progress = TokenProgress {
            input_tokens: 100,
            output_tokens: 20,
        };
        assert_eq!(
            label(TurnPhase::Running, 1_000, 4_000, progress),
            "• Running (3s) (↑100 ↓20)"
        );
        assert_eq!(
            label(TurnPhase::Thinking, 0, 0, TokenProgress::default()),
            "• Thinking (0s)"
        );
        assert_eq!(
            label(
                TurnPhase::Generating,
                0,
                (3600 + 62) * 1000,
                TokenProgress {
                    input_tokens: 2,
                    output_tokens: 0
                }
            ),
            "• Generating (1h1m2s) (↑2 ↓0)"
        );
        assert_eq!(
            label(TurnPhase::Thinking, 0, 125_000, TokenProgress::default()),
            "• Thinking (2m5s)"
        );
    }

    #[test]
    fn the_marker_blinks_with_the_elapsed_seconds() {
        assert!(activity_blink_visible(1_000, 1_000));
        assert!(activity_blink_visible(1_000, 1_499));
        assert!(!activity_blink_visible(1_000, 1_500));
        assert!(activity_blink_visible(1_000, 2_000));
        assert_eq!(activity_phase(1_000, 2_600), 3);
        assert_eq!(activity_phase(1_000, 500), 0);
    }

    #[test]
    fn activity_rows_style_the_label_and_dim_the_token_suffix() {
        let theme = Theme::builtin(false, false, true);
        let progress = TokenProgress {
            input_tokens: 2,
            output_tokens: 28,
        };
        let row = turn_activity_row(&theme, TurnPhase::Generating, 0, 2_000, progress, 80);
        assert_eq!(row.text(), "• Generating (2s) (↑2 ↓28)");
        assert_eq!(row.segments()[0].paint, Paint::fg(252));
        assert_eq!(row.segments()[1].paint, Paint::fg(245));
        let hidden = turn_activity_row(
            &theme,
            TurnPhase::Generating,
            0,
            2_500,
            TokenProgress::default(),
            80,
        );
        assert_eq!(hidden.text(), "  Generating (2s)");
        assert_eq!(hidden.segments().len(), 1);
        assert_eq!(
            turn_activity_row(
                &theme,
                TurnPhase::Thinking,
                0,
                0,
                TokenProgress::default(),
                8
            )
            .text(),
            "• Thi..."
        );
    }

    #[test]
    fn token_progress_settles_reported_usage_over_estimates() {
        let mut tokens = TurnTokens::for_prompt("hello there");
        assert_eq!(tokens.progress().input_tokens, 4);
        tokens.consume_content("abcd efgh");
        assert_eq!(tokens.progress().output_tokens, 2);
        tokens.settle(Some(850));
        assert_eq!(tokens.progress().output_tokens, 850);
        tokens.consume_reasoning("abcdefgh");
        tokens.settle(None);
        assert_eq!(tokens.progress().output_tokens, 852);
        assert_eq!(ProgressSuffix(TokenProgress::default()).to_string(), "");
    }
}

#[cfg(test)]
mod static_status_tests {
    use super::*;

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    #[test]
    fn status_text_wraps_with_a_hanging_indent_up_to_three_rows() {
        let theme = Theme::builtin(false, false, true);
        let rows = static_status_rows(
            "⚠ configured provider authentication failed · HTTP 401 · Check the configured provider auth environment variable.",
            theme.red,
            100,
        );
        assert_eq!(
            texts(&rows),
            [
                "⚠ configured provider authentication failed · HTTP 401 · Check the configured provider auth",
                "  environment variable."
            ]
        );
        assert_eq!(rows[0].segments()[0].paint, theme.red);
    }

    #[test]
    fn overlong_status_text_keeps_the_tail_behind_a_leading_ellipsis() {
        let rows = static_status_rows(
            "⚠ one two three four five six seven eight nine ten",
            Paint::PLAIN,
            14,
        );
        assert_eq!(
            texts(&rows),
            ["⚠ one two", "  three four", "  ... nine ten"]
        );
        assert!(rows.iter().all(|row| row.width() <= 14));
    }

    #[test]
    fn wide_first_characters_never_split_at_width_one() {
        let rows = static_status_rows("界界界界", Paint::PLAIN, 1);
        assert_eq!(texts(&rows), ["界", " 界", " ."]);
        assert_eq!(
            texts(&static_status_rows("fits", Paint::PLAIN, 4)),
            ["fits"]
        );
        assert_eq!(
            texts(&static_status_rows("first\nsecond", Paint::PLAIN, 80)),
            ["first"]
        );
    }
}
