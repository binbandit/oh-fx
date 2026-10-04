use std::cell::RefCell;
use std::cmp::Reverse;
use std::fmt::Write as _;
use std::mem;

use ofx_contract::{CommandProcessPresentation, ToolActivity, ToolCallId};
use ofx_markdown::{highlight, resolve};
use ofx_text::{prefix_by_width, visible_width};

use super::tool_presentation::{ToolActivityRow, ToolOutcome};
use crate::output::activity_status::omission_marker;
use crate::render_engine::display_units::Unit;
use crate::row_text::{Paint, Row};
use crate::theme::Theme;

const CANCELLATION_FOLLOW_UP: &str = " · What can oh-fx do differently?";
const GROUP_MARKER: &str = "●";
const CANCELLATION_MARKER: &str = "■";
const MIDDLE_CONNECTOR: &str = "├";
const LAST_CONNECTOR: &str = "└";
const MIDDLE_CONTINUATION: &str = "│ ";
const LAST_CONTINUATION: &str = "  ";
const SHELL_PROFILE: &str = "sh";
const CATEGORY_LABELS: [&str; 7] = [
    "read", "list", "write", "edit", "open", "command", "subagent",
];
const COMMAND_CATEGORY: usize = 5;
const KNOWN_MULTIWORD_LABELS: [&str; 6] = [
    "Denied by auto agent",
    "Review evidence incomplete",
    "Permission required",
    "Safety caution",
    "Review unavailable",
    "Timed out",
];

#[derive(Debug)]
pub(crate) struct ToolGroup {
    rows: Vec<ToolActivityRow>,
    drawn: RefCell<DrawnRows>,
}

#[derive(Debug, Default)]
struct DrawnRows {
    key: Option<(usize, Theme)>,
    children: Vec<Option<Vec<Row>>>,
}

impl DrawnRows {
    fn forget(&mut self, index: usize) {
        if let Some(child) = self.children.get_mut(index) {
            *child = None;
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Summary {
    total: usize,
    categories: [usize; CATEGORY_LABELS.len()],
    unreported: usize,
    timed_out: usize,
    failed: usize,
    denied: usize,
    cancelled: usize,
}

impl ToolGroup {
    pub(crate) fn new(row: ToolActivityRow) -> Self {
        Self {
            rows: vec![row],
            drawn: RefCell::default(),
        }
    }

    pub(crate) fn push(&mut self, row: ToolActivityRow) {
        self.drawn.get_mut().forget(self.rows.len() - 1);
        self.rows.push(row);
    }

    pub(crate) fn row_mut(&mut self, call_id: &ToolCallId) -> Option<&mut ToolActivityRow> {
        let index = self.rows.iter().rposition(|row| &row.call_id == call_id)?;
        self.drawn.get_mut().forget(index);
        Some(&mut self.rows[index])
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.rows.iter().all(|row| !row.is_active())
    }

    pub(crate) fn settle_active(&mut self, settle: impl Fn(&mut ToolActivityRow)) -> bool {
        let drawn = self.drawn.get_mut();
        let mut settled = false;
        for (index, row) in self.rows.iter_mut().enumerate() {
            if row.is_active() {
                settle(row);
                drawn.forget(index);
                settled = true;
            }
        }
        settled
    }

    pub(crate) fn render(&self, cols: usize, theme: &Theme) -> Vec<Row> {
        let mut rows = vec![header_row(&self.summary(), cols, theme)];
        let mut drawn = self.drawn.borrow_mut();
        if drawn.key != Some((cols, *theme)) {
            drawn.key = Some((cols, *theme));
            drawn.children.clear();
        }
        drawn.children.resize(self.rows.len(), None);
        let last = self.rows.len() - 1;
        for (index, (row, child)) in self.rows.iter().zip(&mut drawn.children).enumerate() {
            let connector = if index == last {
                LAST_CONNECTOR
            } else {
                MIDDLE_CONNECTOR
            };
            rows.extend(
                child
                    .get_or_insert_with(|| child_rows(row, connector, cols, theme))
                    .iter()
                    .cloned(),
            );
        }
        for row in &self.rows {
            if row.status.outcome != Some(ToolOutcome::Cancelled) {
                continue;
            }
            let feedback = cancellation_rows(row, cols, theme);
            if !feedback.is_empty() {
                rows.push(Row::new());
                rows.extend(feedback);
            }
        }
        rows
    }

    fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for row in &self.rows {
            summary.observe(row);
        }
        summary
    }
}

impl Summary {
    fn observe(&mut self, row: &ToolActivityRow) {
        self.total += 1;
        if let Some(index) = row.activity.and_then(category_index) {
            self.categories[index] += 1;
        }
        let command = row.activity == Some(ToolActivity::Command);
        let process = row.status.process.filter(|_| command);
        let timed_out = process == Some(CommandProcessPresentation::TimedOut);
        let process_failed = matches!(
            process,
            Some(
                CommandProcessPresentation::Signal(_)
                    | CommandProcessPresentation::TimedOut
                    | CommandProcessPresentation::OutputCaptureFailed
            )
        ) || matches!(process, Some(CommandProcessPresentation::ExitCode(code)) if code != 0);
        let Some(outcome) = row.status.outcome else {
            return;
        };
        match outcome {
            ToolOutcome::Unreported => self.unreported += 1,
            ToolOutcome::Denied => self.denied += 1,
            ToolOutcome::Cancelled => self.cancelled += 1,
            ToolOutcome::Completed | ToolOutcome::Failed if timed_out => self.timed_out += 1,
            ToolOutcome::Completed | ToolOutcome::Failed
                if outcome == ToolOutcome::Failed || process_failed =>
            {
                self.failed += 1;
            }
            ToolOutcome::Completed | ToolOutcome::Failed | ToolOutcome::Deferred => {}
        }
    }

    fn text(&self) -> String {
        let mut text = format!(
            "{GROUP_MARKER} {} tool call{}",
            self.total,
            if self.total == 1 { "" } else { "s" }
        );
        let mut remaining = self.categories;
        while let Some(index) = (0..remaining.len())
            .filter(|index| remaining[*index] > 0)
            .max_by_key(|index| (remaining[*index], Reverse(*index)))
        {
            let count = mem::take(&mut remaining[index]);
            let label = if index == COMMAND_CATEGORY && count != 1 {
                "commands"
            } else {
                CATEGORY_LABELS[index]
            };
            append_segment(&mut text, count, label);
        }
        append_segment(&mut text, self.unreported, "unreported");
        append_segment(&mut text, self.timed_out, "timed out");
        append_segment(&mut text, self.failed, "failed");
        append_segment(&mut text, self.denied, "denied");
        append_segment(&mut text, self.cancelled, "cancelled");
        text
    }
}

fn category_index(activity: ToolActivity) -> Option<usize> {
    match activity {
        ToolActivity::Read => Some(0),
        ToolActivity::List => Some(1),
        ToolActivity::Write => Some(2),
        ToolActivity::Edit => Some(3),
        ToolActivity::Open => Some(4),
        ToolActivity::Command => Some(COMMAND_CATEGORY),
        ToolActivity::Subagent => Some(6),
        ToolActivity::Ask => None,
    }
}

fn append_segment(text: &mut String, count: usize, label: &str) {
    if count > 0 {
        let _ = write!(text, " · {count} {label}");
    }
}

fn header_row(summary: &Summary, cols: usize, theme: &Theme) -> Row {
    let text = summary.text();
    let clipped = clip_plain(&text, cols);
    let Some(content) = clipped.strip_prefix(GROUP_MARKER) else {
        return Row::plain(&clipped);
    };
    let mut row = Row::styled(GROUP_MARKER, theme.user_card_marker);
    let content = match content.strip_prefix(' ') {
        Some(rest) => {
            row.push(" ", Paint::PLAIN);
            rest
        }
        None => content,
    };
    row.push(content, theme.statusline);
    row
}

fn clip_plain(text: &str, cols: usize) -> String {
    if visible_width(text) <= cols {
        return text.to_owned();
    }
    match cols {
        0 => String::new(),
        1 => "…".to_owned(),
        _ => format!("{}…", prefix_by_width(text, cols - 1)),
    }
}

fn child_rows(row: &ToolActivityRow, connector: &str, cols: usize, theme: &Theme) -> Vec<Row> {
    let mut rows = vec![child_row(row, connector, cols, theme)];
    if let Some(status) = row.child_status() {
        let prefix = if connector == LAST_CONNECTOR {
            LAST_CONTINUATION
        } else {
            MIDDLE_CONTINUATION
        };
        let mut continuation = Row::styled(prefix, theme.statusline);
        continuation.push(status, theme.statusline);
        rows.push(continuation.summary_clipped(cols, theme.statusline));
    }
    rows
}

fn child_row(row: &ToolActivityRow, connector: &str, cols: usize, theme: &Theme) -> Row {
    let phrase = &row.status.phrase;
    let mut child = Row::styled(&format!("{connector} "), theme.statusline);
    let split = (row.activity == Some(ToolActivity::Command))
        .then(|| {
            let display = row.command_display();
            command_split(
                phrase,
                display.as_ref().map(|(label, _)| *label),
                display.as_ref().map(|(_, display)| display.as_str()),
            )
        })
        .flatten();
    match split {
        Some(split) => {
            child.push(&phrase[..=split], theme.statusline);
            push_highlighted(&mut child, &phrase[split + 1..], theme.statusline, theme);
        }
        None if row.shows_diff_stats() && child.width() + visible_width(phrase) <= cols => {
            push_accented_stats(&mut child, phrase, theme);
            return child;
        }
        None => child.push(phrase, theme.statusline),
    }
    child.summary_clipped(cols, theme.statusline)
}

fn push_accented_stats(row: &mut Row, phrase: &str, theme: &Theme) {
    let base = theme.statusline;
    let (Some((added, removed)), Some(last)) =
        (theme.diff_marker_paints(), trailing_stat_token(phrase))
    else {
        row.push(phrase, base);
        return;
    };
    if !last.added && last.start >= 2 && phrase.as_bytes()[last.start - 2] == b'/' {
        let before_slash = phrase[..last.start - 2].trim_end_matches(' ');
        if let Some(first) = trailing_stat_token(before_slash).filter(|first| first.added) {
            row.push(&before_slash[..first.start], base);
            row.push(&before_slash[first.start..], added);
            row.push(" / ", base);
            row.push(&phrase[last.start..], removed);
            return;
        }
    }
    row.push(&phrase[..last.start], base);
    row.push(
        &phrase[last.start..],
        if last.added { added } else { removed },
    );
}

struct StatToken {
    start: usize,
    added: bool,
}

fn trailing_stat_token(text: &str) -> Option<StatToken> {
    let bytes = text.as_bytes();
    let digits = bytes
        .iter()
        .rev()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let index = bytes.len() - digits;
    if digits == 0 || index < 2 || bytes[index - 2] != b' ' {
        return None;
    }
    match bytes[index - 1] {
        b'+' => Some(StatToken {
            start: index - 1,
            added: true,
        }),
        b'-' => Some(StatToken {
            start: index - 1,
            added: false,
        }),
        _ => None,
    }
}

fn command_split(phrase: &str, action_label: Option<&str>, display: Option<&str>) -> Option<usize> {
    let label_end = action_label
        .filter(|label| phrase.len() > label.len() && phrase.starts_with(label))
        .map(str::len)
        .filter(|end| phrase.as_bytes()[*end] == b' ');
    let display_end = display
        .filter(|display| !display.is_empty() && phrase.len() > display.len() + 1)
        .filter(|display| phrase.ends_with(display))
        .map(|display| phrase.len() - display.len() - 1)
        .filter(|split| phrase.as_bytes()[*split] == b' ');
    let split = label_end
        .or(display_end)
        .or_else(|| known_label_prefix(phrase))
        .or_else(|| phrase.find(' '))?;
    (split + 1 < phrase.len()).then_some(split)
}

fn known_label_prefix(phrase: &str) -> Option<usize> {
    KNOWN_MULTIWORD_LABELS.iter().find_map(|label| {
        (phrase.len() > label.len()
            && phrase.starts_with(label)
            && phrase.as_bytes()[label.len()] == b' ')
            .then_some(label.len())
    })
}

fn push_highlighted(row: &mut Row, command: &str, base: Paint, theme: &Theme) {
    let Some(profile) = resolve(SHELL_PROFILE) else {
        row.push(command, base);
        return;
    };
    for line in highlight(command, profile, None) {
        for span in &line.spans {
            let paint = if span.style.slot.is_some() {
                theme.markdown(span.style)
            } else {
                base
            };
            row.push(&span.text, paint);
        }
    }
}

fn cancellation_rows(row: &ToolActivityRow, cols: usize, theme: &Theme) -> Vec<Row> {
    let mut line = Row::styled(CANCELLATION_MARKER, theme.warning);
    let phrase = &row.status.phrase;
    if let Some((target, highlighted)) = row.cancellation_target() {
        line.push(" ", theme.hint);
        line.push(&phrase[..row.status.label_len], theme.hint);
        line.push(" ", Paint::PLAIN);
        if highlighted {
            push_highlighted(&mut line, &target, theme.tool_stdout, theme);
        } else {
            line.push(&target, theme.tool_stdout);
        }
    } else {
        line.push(" ", Paint::PLAIN);
        line.push(phrase, theme.hint);
    }
    line.push(CANCELLATION_FOLLOW_UP, Paint::PLAIN);
    status_preview(&line, cols)
}

struct StatusLine {
    end: usize,
    next: usize,
    omitted: bool,
}

fn status_preview(line: &Row, cols: usize) -> Vec<Row> {
    let units = line.units();
    if cols == 0 || units.is_empty() {
        return Vec::new();
    }
    let first = scan_status_line(&units, 0, cols);
    let mut rows = vec![Row::from_units(&units[..first.end])];
    if first.next >= units.len() && !first.omitted {
        return rows;
    }
    let indent = if cols >= 6 { 2 } else { 0 };
    let available = cols - indent;
    let full = scan_status_line(&units, first.next, available);
    let mut second = Row::new();
    second.push_spaces(indent);
    if !first.omitted && full.next >= units.len() && !full.omitted {
        for unit in &units[first.next..full.end] {
            second.push_linked(unit.text, unit.paint, unit.link);
        }
        rows.push(second);
        return rows;
    }
    let marker = omission_marker(cols);
    let clipped = scan_status_line(&units, first.next, available - marker.len());
    for unit in &units[first.next..clipped.end] {
        second.push_linked(unit.text, unit.paint, unit.link);
    }
    let marker_paint = clipped
        .end
        .checked_sub(1)
        .map_or(Paint::PLAIN, |index| units[index].paint);
    second.push(marker, marker_paint);
    rows.push(second);
    rows
}

fn scan_status_line(units: &[Unit<'_>], start: usize, max_width: usize) -> StatusLine {
    let unchanged = StatusLine {
        end: start,
        next: start,
        omitted: false,
    };
    if start >= units.len() || max_width == 0 {
        return unchanged;
    }
    let mut index = start;
    let mut width = 0;
    let mut last_wrap: Option<(usize, usize)> = None;
    while index < units.len() {
        let unit = &units[index];
        if unit.is_space() {
            let next = skip_spaces(units, index);
            if width + 1 > max_width {
                return StatusLine {
                    end: index,
                    next,
                    omitted: false,
                };
            }
            last_wrap = Some((index, next));
            width += 1;
            index += 1;
            continue;
        }
        if width + unit.width > max_width {
            if let Some((end, next)) = last_wrap {
                if token_exceeds_width(units, next, max_width) {
                    return StatusLine {
                        end: index,
                        next: index,
                        omitted: false,
                    };
                }
                return StatusLine {
                    end,
                    next,
                    omitted: false,
                };
            }
            if width == 0 {
                return StatusLine {
                    end: index,
                    next: index + 1,
                    omitted: true,
                };
            }
            return StatusLine {
                end: index,
                next: index,
                omitted: false,
            };
        }
        width += unit.width;
        index += 1;
    }
    StatusLine {
        end: index,
        next: index,
        omitted: false,
    }
}

fn skip_spaces(units: &[Unit<'_>], start: usize) -> usize {
    units[start..]
        .iter()
        .position(|unit| !unit.is_space())
        .map_or(units.len(), |offset| start + offset)
}

fn token_exceeds_width(units: &[Unit<'_>], start: usize, max_width: usize) -> bool {
    let mut width = 0;
    for unit in &units[start..] {
        if unit.is_space() {
            return false;
        }
        if width + unit.width > max_width {
            return true;
        }
        width += unit.width;
    }
    false
}

#[cfg(test)]
mod tests {
    use ofx_contract::{
        ActionLabel, CallDescription, Concurrency, FileChangeStats, ToolEffect, ToolResultStatus,
        TurnOutcome, tool_permission_denied_json,
    };

    use super::super::tool_presentation::Finished;
    use super::*;
    use crate::row_text::Paint;

    const ESC: char = '\u{1b}';

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    fn started(
        id: &str,
        tool: &str,
        activity: ToolActivity,
        label: (&'static str, &'static str, &str),
    ) -> ToolActivityRow {
        ToolActivityRow::started(
            ToolCallId::new(id),
            tool,
            CallDescription {
                title: format!("{} {}", label.0, label.2),
                label: Some(ActionLabel {
                    active: label.0,
                    completed: label.1,
                    target: label.2.to_owned(),
                }),
                activity,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Parallel,
            },
        )
    }

    fn settled(
        mut row: ToolActivityRow,
        status: ToolResultStatus,
        content: &str,
        process: Option<CommandProcessPresentation>,
    ) -> ToolActivityRow {
        row.finish(&Finished {
            arguments: "{}",
            status,
            content,
            process,
            status_detail: None,
            file_change: None,
        });
        row
    }

    fn read(id: &str, target: &str) -> ToolActivityRow {
        settled(
            started(
                id,
                "read_file",
                ToolActivity::Read,
                ("Reading", "Read", target),
            ),
            ToolResultStatus::Success,
            "",
            None,
        )
    }

    fn command(
        id: &str,
        target: &str,
        process: Option<CommandProcessPresentation>,
    ) -> ToolActivityRow {
        settled(
            started(
                id,
                "shell",
                ToolActivity::Command,
                ("Running", "Ran", target),
            ),
            ToolResultStatus::Success,
            "",
            process,
        )
    }

    fn group(rows: Vec<ToolActivityRow>) -> ToolGroup {
        let mut rows = rows.into_iter();
        let mut group = ToolGroup::new(rows.next().unwrap());
        for row in rows {
            group.push(row);
        }
        group
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    fn painted(row: &Row, text: &str) -> Paint {
        let segment = row.segments().iter().find(|segment| segment.text == text);
        segment
            .unwrap_or_else(|| panic!("{text:?} in {:?}", row.segments()))
            .paint
    }

    #[test]
    fn group_summary_uses_semantic_category_order_and_outcomes() {
        let edit = settled(
            started(
                "2",
                "edit_file",
                ToolActivity::Edit,
                ("Editing", "Edited", "main.zig"),
            ),
            ToolResultStatus::Failure,
            "edit_file failed: boom",
            None,
        );
        let rendered = group(vec![
            read("1", "runtime.zig"),
            edit,
            command("3", "zig build", None),
        ])
        .render(120, &theme());
        assert_eq!(
            texts(&rendered),
            [
                "● 3 tool calls · 1 read · 1 edit · 1 command · 1 failed",
                "├ Read runtime.zig",
                "├ Failed main.zig",
                "└ Ran zig build",
            ]
        );
        let header = &rendered[0];
        assert_eq!(painted(header, "●"), Paint::fg(255));
        assert_eq!(painted(header, " "), Paint::PLAIN);
        assert_eq!(
            painted(
                header,
                "3 tool calls · 1 read · 1 edit · 1 command · 1 failed"
            ),
            Paint::fg(245)
        );
        assert_eq!(painted(&rendered[1], "├ Read runtime.zig"), Paint::fg(245));
        assert_eq!(painted(&rendered[3], "zig"), Paint::fg(252));
        assert_eq!(painted(&rendered[3], " build"), Paint::fg(245));
    }

    #[test]
    fn the_largest_categories_lead_and_commands_pluralize() {
        let rows = vec![
            command("1", "a", None),
            read("2", "x"),
            command("3", "b", Some(CommandProcessPresentation::ExitCode(1))),
            settled(
                started(
                    "4",
                    "glob_files",
                    ToolActivity::List,
                    ("Matching", "Matched", "*.rs"),
                ),
                ToolResultStatus::Success,
                "",
                None,
            ),
            command("5", "sleep 5", Some(CommandProcessPresentation::TimedOut)),
        ];
        assert_eq!(
            group(rows).render(120, &theme())[0].text(),
            "● 5 tool calls · 3 commands · 1 read · 1 list · 1 timed out · 1 failed"
        );
        assert_eq!(
            group(vec![command("1", "a", None)]).render(120, &theme())[0].text(),
            "● 1 tool call · 1 command"
        );
    }

    #[test]
    fn command_rows_highlight_the_command_without_coloring_the_action() {
        let rendered = group(vec![
            started(
                "1",
                "shell",
                ToolActivity::Command,
                ("Running", "Ran", "rg snapshot"),
            ),
            command("2", "cat log.txt | head -80", None),
            command(
                "3",
                "printf 'hello world'",
                Some(CommandProcessPresentation::ExitCode(7)),
            ),
            command("4", "sleep 5", Some(CommandProcessPresentation::TimedOut)),
        ])
        .render(120, &theme());
        assert_eq!(
            texts(&rendered),
            [
                "● 4 tool calls · 4 commands · 1 timed out · 1 failed",
                "├ Running rg snapshot",
                "├ Ran cat log.txt | head -80",
                "├ Exited 7 printf 'hello world'",
                "└ Timed out sleep 5",
            ]
        );
        assert_eq!(painted(&rendered[1], "├ Running "), Paint::fg(245));
        assert_eq!(painted(&rendered[1], "rg"), Paint::fg(252));
        assert_eq!(painted(&rendered[2], "|"), Paint::fg(252));
        assert_eq!(painted(&rendered[2], "head"), Paint::fg(252));
        assert_eq!(painted(&rendered[2], "-80"), Paint::fg(250));
        assert_eq!(painted(&rendered[3], "├ Exited 7 "), Paint::fg(245));
        assert_eq!(painted(&rendered[3], "'hello world'"), Paint::fg(250));
        assert_eq!(painted(&rendered[4], "└ Timed out "), Paint::fg(245));
        assert_eq!(painted(&rendered[4], "sleep"), Paint::fg(252));
    }

    #[test]
    fn command_split_prefers_the_label_then_the_display_then_known_labels() {
        let cases = [
            ("Ran zig build", Some("Ran"), Some("zig build"), Some(3)),
            ("Exited 7 exit 7", Some("Ran"), Some("exit 7"), Some(8)),
            ("Timed out sleep 5", None, None, Some(9)),
            ("Review unavailable rm x", None, None, Some(18)),
            (
                "Failed echo hi · 1 invalid field",
                Some("Ran"),
                Some("echo hi"),
                Some(6),
            ),
            ("Ran", Some("Ran"), Some("Ran"), None),
            ("Failed ", None, None, None),
            ("single", None, None, None),
        ];
        for (phrase, label, display, expected) in cases {
            assert_eq!(command_split(phrase, label, display), expected, "{phrase}");
        }
    }

    #[test]
    fn rows_clip_to_the_width_with_an_ellipsis_in_the_style_at_the_cut() {
        let long = format!("printf {}", "alpha-beta-gamma-delta-".repeat(8));
        let rows = group(vec![read("1", &"a".repeat(200)), command("2", &long, None)]);
        for cols in [1, 2, 10, 24, 80] {
            let rendered = rows.render(cols, &theme());
            assert_eq!(rendered.len(), 3, "{cols}");
            for row in &rendered {
                assert!(row.width() <= cols, "{cols} {:?}", row.text());
            }
        }
        let narrow = rows.render(24, &theme());
        assert_eq!(narrow[0].text(), "● 2 tool calls · 1 read…");
        assert_eq!(narrow[2].text(), "└ Ran printf alpha-beta…");
        assert_eq!(painted(&narrow[2], "printf"), Paint::fg(252));
        let wide = rows.render(240, &theme());
        assert_eq!(wide[2].text(), format!("└ Ran {long}"));
        let tiny = rows.render(1, &theme());
        assert_eq!(texts(&tiny), ["…", "…", "…"]);
        assert_eq!(painted(&tiny[0], "…"), Paint::PLAIN);
        assert_eq!(painted(&tiny[1], "…"), Paint::fg(245));
        assert!(
            texts(&rows.render(0, &theme()))
                .iter()
                .all(String::is_empty)
        );
        let token_cut = group(vec![command(
            "1",
            "printf 'a quoted string that runs on'",
            None,
        )]);
        let cut = &token_cut.render(20, &theme())[1];
        assert_eq!(cut.text(), "└ Ran printf 'a quo…");
        assert_eq!(painted(cut, "'a quo…"), Paint::fg(250));
    }

    #[test]
    fn wide_and_ambiguous_characters_clip_by_their_cells() {
        let rows = group(vec![
            read("1", "日本語のファイル名がとても長い.txt"),
            read("2", "😀😀😀😀😀😀😀😀😀😀"),
        ]);
        for cols in [5, 9, 12, 17] {
            for row in rows.render(cols, &theme()) {
                assert!(row.width() <= cols, "{cols} {:?}", row.text());
            }
        }
        assert_eq!(rows.render(12, &theme())[1].text(), "├ Read 日本…");
    }

    #[test]
    fn cancelled_rows_add_feedback_after_the_children() {
        let mut active = started(
            "2",
            "shell",
            ToolActivity::Command,
            ("Running", "Ran", "sleep 8; echo done"),
        );
        active.cancel();
        let rendered = group(vec![read("1", "README.md"), active]).render(100, &theme());
        assert_eq!(
            texts(&rendered),
            [
                "● 2 tool calls · 1 read · 1 command · 1 cancelled",
                "├ Read README.md",
                "└ Cancelled sleep 8; echo done",
                "",
                "■ Cancelled sleep 8; echo done · What can oh-fx do differently?",
            ]
        );
        let feedback = &rendered[4];
        assert_eq!(painted(feedback, "■"), Paint::fg(252));
        assert_eq!(painted(feedback, " Cancelled"), Paint::fg(255));
        assert_eq!(painted(feedback, "sleep"), Paint::fg(252));
        assert_eq!(painted(feedback, " 8"), Paint::fg(245));
        assert_eq!(
            painted(feedback, " · What can oh-fx do differently?"),
            Paint::PLAIN
        );
        let mut fallback = started(
            "1",
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", "a"),
        );
        fallback.abandon(TurnOutcome::Interrupted);
        let rendered = group(vec![fallback]).render(100, &theme());
        assert_eq!(
            rendered[3].text(),
            "■ Tool cancelled · What can oh-fx do differently?"
        );
        assert_eq!(painted(&rendered[3], "Tool cancelled"), Paint::fg(255));
    }

    #[test]
    fn cancellation_feedback_wraps_to_at_most_two_rows() {
        let mut long = started(
            "1",
            "shell",
            ToolActivity::Command,
            (
                "Running",
                "Ran",
                "sleep waiting command words extend past line two",
            ),
        );
        long.cancel();
        let rendered = group(vec![long]).render(24, &theme());
        let gap = rendered.iter().position(|row| row.width() == 0).unwrap();
        assert_eq!(gap, 2);
        assert_eq!(
            texts(&rendered[gap + 1..]),
            ["■ Cancelled sleep", "  waiting command..."]
        );
        for row in &rendered {
            assert!(row.width() <= 24, "{:?}", row.text());
        }
        let mut short = started(
            "1",
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", "a.txt"),
        );
        short.cancel();
        let rendered = group(vec![short]).render(30, &theme());
        assert_eq!(
            texts(&rendered[3..]),
            ["■ Cancelled a.txt · What can", "  oh-fx do differently?"]
        );
        let mut unbroken = started(
            "1",
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", &"x".repeat(40)),
        );
        unbroken.cancel();
        let rendered = group(vec![unbroken]).render(20, &theme());
        assert_eq!(
            texts(&rendered[3..]),
            ["■ Cancelled xxxxxxxx", "  xxxxxxxxxxxxxxx..."]
        );
    }

    fn changed(
        id: &str,
        tool: &str,
        activity: ToolActivity,
        label: (&'static str, &'static str, &str),
        change: (u32, u32),
    ) -> ToolActivityRow {
        let mut row = started(id, tool, activity, label);
        row.finish(&Finished {
            arguments: "{}",
            status: ToolResultStatus::Success,
            content: "",
            process: None,
            status_detail: None,
            file_change: Some(FileChangeStats {
                additions: change.0,
                deletions: change.1,
            }),
        });
        row
    }

    #[test]
    fn file_change_counts_take_the_marker_accents_of_a_pinned_theme() {
        let rows = group(vec![
            read("1", "runtime.zig"),
            changed(
                "2",
                "write_file",
                ToolActivity::Write,
                ("Writing", "Wrote", "note.txt"),
                (143, 0),
            ),
            changed(
                "3",
                "edit_file",
                ToolActivity::Edit,
                ("Editing", "Edited", "main.zig"),
                (12, 3),
            ),
            changed(
                "4",
                "edit_file",
                ToolActivity::Edit,
                ("Editing", "Edited", "gone.zig"),
                (0, 27),
            ),
            read("5", "notes +7"),
        ]);
        let pinned = Theme::builtin(false, true, false);
        let rendered = rows.render(120, &pinned);
        assert_eq!(
            texts(&rendered),
            [
                "● 5 tool calls · 2 read · 2 edit · 1 write",
                "├ Read runtime.zig",
                "├ Wrote note.txt +143",
                "├ Edited main.zig +12 / -3",
                "├ Edited gone.zig -27",
                "└ Read notes +7",
            ]
        );
        let added = Paint::fg(71);
        let removed = Paint::fg(167);
        assert_eq!(painted(&rendered[2], "+143"), added);
        assert_eq!(painted(&rendered[3], "+12"), added);
        assert_eq!(painted(&rendered[3], " / "), Paint::fg(245));
        assert_eq!(painted(&rendered[3], "-3"), removed);
        assert_eq!(painted(&rendered[4], "-27"), removed);
        assert_eq!(painted(&rendered[5], "└ Read notes +7"), Paint::fg(245));
        let following = rows.render(120, &theme());
        assert_eq!(
            painted(&following[3], "├ Edited main.zig +12 / -3"),
            Paint::fg(245)
        );
        let clipped = rows.render(16, &pinned);
        assert_eq!(clipped[3].text(), "├ Edited main.z…");
        assert_eq!(clipped[3].segments().len(), 1);
    }

    #[test]
    fn denials_count_separately_from_failures() {
        let denied = settled(
            started(
                "1",
                "shell",
                ToolActivity::Command,
                ("Running", "Ran", "touch made.txt"),
            ),
            ToolResultStatus::Failure,
            &tool_permission_denied_json("shell"),
            None,
        );
        let held = settled(
            started(
                "3",
                "shell",
                ToolActivity::Command,
                ("Running", "Ran", "zig build"),
            ),
            ToolResultStatus::Failure,
            r#"{"error":{"type":"tool_review_held","reason":"review_caution","held":true}}"#,
            None,
        );
        let rendered = group(vec![read("2", "README.md"), denied, held]).render(100, &theme());
        assert_eq!(
            texts(&rendered),
            [
                "● 3 tool calls · 2 commands · 1 read · 2 denied",
                "├ Read README.md",
                "├ Denied touch made.txt",
                "└ Safety caution zig build",
            ]
        );
        assert_eq!(painted(&rendered[3], "└ Safety caution "), Paint::fg(245));
        assert_eq!(painted(&rendered[3], "zig"), Paint::fg(252));
    }

    #[test]
    fn hostile_targets_render_as_visible_escapes() {
        let rows = group(vec![
            read("1", "a\x1b]2;owned\x07b"),
            command("2", "printf '\x1b[2J\x1b[31mred'", None),
        ]);
        for row in rows.render(200, &theme()) {
            let encoded = row.encode();
            let stripped: String = encoded
                .split(ESC)
                .enumerate()
                .map(|(index, part)| {
                    if index == 0 {
                        part
                    } else {
                        part.split_once('m').map_or("", |(_, rest)| rest)
                    }
                })
                .collect();
            assert!(
                !stripped.contains(ESC) && !stripped.contains('\x07'),
                "{encoded:?}"
            );
            assert!(
                !encoded.contains("\x1b]") && !encoded.contains("\x1b[2J"),
                "{encoded:?}"
            );
        }
        assert_eq!(
            rows.render(200, &theme())[1].text(),
            "├ Read a\\x1b]2;owned\\x07b"
        );
    }

    #[test]
    fn groups_settle_only_once_every_row_has_an_outcome() {
        let mut rows = group(vec![started(
            "1",
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", "a"),
        )]);
        assert!(!rows.is_settled());
        rows.push(read("2", "b"));
        assert!(!rows.is_settled());
        assert!(rows.settle_active(ToolActivityRow::cancel));
        assert!(rows.is_settled());
        assert!(!rows.settle_active(ToolActivityRow::cancel));
        assert!(!rows.settle_active(|row| row.abandon(TurnOutcome::Completed)));
        let mut unreported = group(vec![started(
            "1",
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", "a"),
        )]);
        assert!(unreported.settle_active(|row| row.abandon(TurnOutcome::Completed)));
        assert_eq!(
            texts(&unreported.render(100, &theme())),
            [
                "● 1 tool call · 1 read · 1 unreported",
                "└ Tool completion was not reported",
            ]
        );
        let row = unreported.row_mut(&ToolCallId::new("1")).unwrap();
        assert!(!row.is_active());
        assert!(unreported.row_mut(&ToolCallId::new("9")).is_none());
    }

    fn fresh(group: &ToolGroup) -> ToolGroup {
        let mut rows = group.rows.iter().cloned();
        let mut copy = ToolGroup::new(rows.next().unwrap());
        for row in rows {
            copy.push(row);
        }
        copy
    }

    #[test]
    fn cached_rows_redraw_whatever_changed_since_the_last_render() {
        let dark = theme();
        let light = Theme::builtin(true, false, true);
        let mut rows = group(vec![command("1", "cat log.txt | head -80", None)]);
        let check = |rows: &ToolGroup, cols, theme: &Theme| {
            assert_eq!(rows.render(cols, theme), fresh(rows).render(cols, theme));
        };
        check(&rows, 100, &dark);
        rows.push(started(
            "2",
            "read_file",
            ToolActivity::Read,
            ("Reading", "Read", "notes.md"),
        ));
        check(&rows, 100, &dark);
        rows.row_mut(&ToolCallId::new("2"))
            .unwrap()
            .finish(&Finished {
                arguments: "{}",
                status: ToolResultStatus::Failure,
                content: "",
                process: None,
                status_detail: None,
                file_change: None,
            });
        check(&rows, 100, &dark);
        rows.push(started(
            "3",
            "shell",
            ToolActivity::Command,
            ("Running", "Ran", "sleep 8"),
        ));
        check(&rows, 100, &dark);
        assert!(rows.settle_active(ToolActivityRow::cancel));
        check(&rows, 100, &dark);
        check(&rows, 12, &dark);
        check(&rows, 12, &light);
        check(&rows, 100, &light);
        assert_eq!(rows.render(100, &light)[3].text(), "└ Cancelled sleep 8");
    }

    #[test]
    fn tool_heavy_groups_render_every_action_within_the_width() {
        let rows: Vec<ToolActivityRow> = (0..20)
            .map(|index| match index {
                0..10 => read(&index.to_string(), "file.zig"),
                10..17 => command(&index.to_string(), "zig build", None),
                17 => command(
                    &index.to_string(),
                    "rg snapshot",
                    Some(CommandProcessPresentation::ExitCode(1)),
                ),
                _ => started(
                    &index.to_string(),
                    "edit_file",
                    ToolActivity::Edit,
                    ("Editing", "Edited", "runtime.zig"),
                ),
            })
            .collect();
        let rendered = group(rows).render(100, &theme());
        assert_eq!(rendered.len(), 21);
        assert_eq!(
            rendered[0].text(),
            "● 20 tool calls · 10 read · 8 commands · 2 edit · 1 failed"
        );
        assert_eq!(rendered[18].text(), "├ Exited 1 rg snapshot");
        assert_eq!(rendered[20].text(), "└ Editing runtime.zig");
        assert!(rendered.iter().all(|row| row.width() <= 100));
    }
}
