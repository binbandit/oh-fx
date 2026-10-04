use ofx_contract::{Notice, NoticeTone};
use ofx_markdown::{
    CodeBlockPayload, DiffMarkers, Event, Line, Profile, highlight, highlight_diff, infer,
    render_table_payload, resolve,
};
use ofx_text::{prefix_by_width, visible_width};

use super::assistant_wrap::{gutter_width, wrap_line};
use super::display_units::{Unit, display_units};
use crate::assistant::user_message_card::user_prompt_card;
use crate::footer::question_ui::{cancelled_resolution_row, resolution_rows};
use crate::output::activity_status::{ProgressSuffix, TokenProgress, static_status_rows};
use crate::render::welcome_rows;
use crate::row_text::{Paint, Row, terminal_safe, terminal_safe_keeping_breaks};
use crate::theme::Theme;
use crate::transcript::tool_group_projection::ToolGroup;

#[derive(Debug)]
pub(crate) enum Entry {
    Welcome {
        version: String,
    },
    UserTurn {
        text: String,
    },
    Assistant {
        events: Vec<Event>,
    },
    Notice(Notice),
    TurnSummary {
        duration_ms: u64,
        progress: TokenProgress,
    },
    Cancellation,
    QuestionResolution {
        answers: Vec<(String, String)>,
    },
    QuestionCancelled,
    TurnFailure {
        text: String,
    },
    ToolGroup(ToolGroup),
}

impl Entry {
    pub(crate) fn keeps_trailing_blank(&self) -> bool {
        matches!(self, Self::Welcome { .. })
    }

    pub(crate) fn wants_footer_gap(&self) -> bool {
        !matches!(self, Self::Welcome { .. } | Self::UserTurn { .. })
    }

    pub(crate) fn render(&self, cols: usize, theme: &Theme) -> Vec<Row> {
        match self {
            Self::Welcome { version } => welcome_rows(theme, version, cols),
            Self::UserTurn { text } => user_prompt_card(text, cols, theme),
            Self::Assistant { events } => events[..events.len() - trailing_blank_lines(events)]
                .iter()
                .flat_map(|event| render_assistant_event(event, cols, theme))
                .collect(),
            Self::Notice(notice) => render_semantic_notice(notice, cols, theme),
            Self::TurnSummary {
                duration_ms,
                progress,
            } => vec![turn_summary_row(*duration_ms, *progress, theme).clipped(cols)],
            Self::Cancellation => vec![cancellation_row(theme).clipped(cols)],
            Self::QuestionResolution { answers } => resolution_rows(theme, answers, cols),
            Self::QuestionCancelled => vec![cancelled_resolution_row(theme).clipped(cols)],
            Self::TurnFailure { text } => static_status_rows(text, theme.red, cols),
            Self::ToolGroup(group) => group.render(cols, theme),
        }
    }
}

pub(crate) fn is_blank_line(event: &Event) -> bool {
    matches!(event, Event::Line(line) if line.spans.iter().all(|span| span.text.trim_matches([' ', '\t']).is_empty()))
}

pub(crate) fn trailing_blank_lines(events: &[Event]) -> usize {
    events
        .iter()
        .rev()
        .take_while(|event| is_blank_line(event))
        .count()
}

pub(crate) fn render_assistant_event(event: &Event, cols: usize, theme: &Theme) -> Vec<Row> {
    let gutter = gutter_width(cols);
    match event {
        Event::Line(line) => wrap_line(line, cols, gutter, theme),
        Event::CodeBlock(block) => code_panel_rows(block, cols, theme),
        Event::Table(table) => render_table_payload(table)
            .iter()
            .flat_map(|line| wrap_line(line, usize::MAX, gutter, theme))
            .map(|row| row.clipped(cols))
            .collect(),
        Event::ThematicRule => {
            let mut row = Row::new();
            row.push_spaces(gutter);
            row.push(
                &"─".repeat(cols.saturating_sub(gutter)),
                Paint::PLAIN.with_dim(),
            );
            vec![row]
        }
    }
}

const UNBOXED_CODE_COLS: usize = 5;
const PLAIN_TEXT_PROFILE: &str = "text";

fn code_panel_rows(block: &CodeBlockPayload, cols: usize, theme: &Theme) -> Vec<Row> {
    let gutter = gutter_width(cols);
    let inner = cols.saturating_sub(gutter);
    if inner == 0 {
        return Vec::new();
    }
    let code = block.code.replace('\t', "    ");
    let code = code.strip_suffix('\n').unwrap_or(&code);
    let profile = if block.language.is_empty() {
        infer(code)
    } else {
        resolve(&block.language)
    };
    let language = if block.language.is_empty() {
        profile.map_or("", Profile::label)
    } else {
        block.language.as_str()
    };
    let lines = code_lines(code, profile, theme);
    let mut rows = Vec::new();
    if inner <= UNBOXED_CODE_COLS {
        for line in &lines {
            rows.extend(wrap_code_line(line, inner, theme));
        }
        return with_gutter(rows, gutter);
    }
    let max_code_width = code.split('\n').map(visible_width).max().unwrap_or(0);
    let label_width = if language.is_empty() {
        0
    } else {
        visible_width(language).min(inner - 4)
    };
    let panel_width = inner.min(6.max(max_code_width.max(label_width + 4)));
    if language.is_empty() {
        rows.push(Row::styled(
            &"─".repeat(panel_width),
            Paint::PLAIN.with_dim(),
        ));
    } else {
        let label = prefix_by_width(language, panel_width - 4);
        let (label, width) = if label.is_empty() {
            ("?", 1)
        } else {
            (label, visible_width(label))
        };
        let header = format!("─ {label} {}", "─".repeat(panel_width - 3 - width));
        rows.push(Row::styled(&header, Paint::PLAIN.with_dim()));
    }
    for line in &lines {
        rows.extend(wrap_code_line(line, panel_width, theme));
    }
    if lines.is_empty() {
        rows.push(Row::new());
    }
    rows.push(Row::styled(
        &"─".repeat(panel_width),
        Paint::PLAIN.with_dim(),
    ));
    with_gutter(rows, gutter)
}

fn code_lines(code: &str, profile: Option<&Profile>, theme: &Theme) -> Vec<Line> {
    if code.is_empty() {
        return Vec::new();
    }
    match profile.or_else(|| resolve(PLAIN_TEXT_PROFILE)) {
        Some(profile) if profile.diff_lines() => {
            let markers = if theme.accented_diff_markers() {
                DiffMarkers::Accented
            } else {
                DiffMarkers::Unstyled
            };
            highlight_diff(code, markers)
        }
        Some(profile) => highlight(code, profile, None),
        None => Vec::new(),
    }
}

fn wrap_code_line(line: &Line, width: usize, theme: &Theme) -> Vec<Row> {
    let units: Vec<Unit<'_>> = line
        .spans
        .iter()
        .flat_map(|span| display_units(&span.text, theme.markdown(span.style), None))
        .collect();
    if units.is_empty() {
        return vec![Row::new()];
    }
    let indent = units.iter().take_while(|unit| unit.is_space()).count();
    let mut rows = Vec::new();
    let mut row = Row::new();
    let mut used = 0;
    for unit in units {
        if used + unit.width > width && used > 0 {
            rows.push(std::mem::take(&mut row));
            used = 0;
            if indent < width && indent + unit.width <= width {
                row.push_spaces(indent);
                used = indent;
            }
        }
        if unit.width > width {
            row.push("?", unit.paint);
            used += 1;
            continue;
        }
        row.push(unit.text, unit.paint);
        used += unit.width;
    }
    rows.push(row);
    rows
}

fn with_gutter(mut rows: Vec<Row>, gutter: usize) -> Vec<Row> {
    for row in &mut rows {
        if row.width() > 0 {
            row.indent(gutter);
        }
    }
    rows
}

fn format_duration_compact(duration_ms: u64) -> String {
    let total_seconds = duration_ms / 1000;
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    if total_seconds < 60 * 60 {
        return format!("{}m {}s", total_seconds / 60, total_seconds % 60);
    }
    format!(
        "{}h {:02}m",
        total_seconds / 3600,
        (total_seconds % 3600) / 60
    )
}

pub(crate) fn turn_summary_row(duration_ms: u64, progress: TokenProgress, theme: &Theme) -> Row {
    let text = format!(
        "  {}{}",
        format_duration_compact(duration_ms),
        ProgressSuffix(progress)
    );
    Row::styled(&text, theme.dim)
}

pub(crate) fn cancellation_row(theme: &Theme) -> Row {
    let mut row = Row::styled("■", theme.warning);
    row.push(" ", Paint::PLAIN);
    row.push("Cancelled", theme.hint);
    row.push(" · What can oh-fx do differently?", Paint::PLAIN);
    row
}

fn notice_glyph(tone: NoticeTone) -> &'static str {
    match tone {
        NoticeTone::Information => "i",
        NoticeTone::Success => "✓",
        NoticeTone::Warning => "!",
        NoticeTone::Error => "✗",
        NoticeTone::Cancelled => "⊘",
        NoticeTone::Neutral => "*",
    }
}

fn notice_label_style(theme: &Theme, tone: NoticeTone) -> Paint {
    match tone {
        NoticeTone::Information => theme.system_notice_label,
        NoticeTone::Success => theme.green,
        NoticeTone::Warning => theme.warning,
        NoticeTone::Error => theme.red,
        NoticeTone::Cancelled => theme.dim,
        NoticeTone::Neutral => theme.system_notice_text,
    }
}

fn notice_label(notice: &Notice) -> String {
    let mut label = format!("{} ", notice_glyph(notice.tone));
    if !notice.topic.is_empty() {
        label.push_str(&notice.topic);
        label.push(':');
    }
    label
}

fn notice_cells<'a>(
    notice: &Notice,
    label: &'a str,
    body: &'a str,
    link: Option<(&'a str, &'a str)>,
    theme: &Theme,
) -> Vec<Unit<'a>> {
    let label_paint = notice_label_style(theme, notice.tone);
    let body_paint = theme.system_notice_text;
    let mut cells: Vec<Unit<'a>> = display_units(label, label_paint, None).collect();
    if !body.is_empty() {
        if !notice.topic.is_empty() {
            cells.extend(display_units(" ", body_paint, None));
        }
        cells.extend(display_units(body, body_paint, None));
    }
    if let Some((link_label, link_target)) = link {
        cells.extend(display_units(" (", body_paint, None));
        cells.extend(display_units(
            link_label,
            body_paint.with(crate::row_text::Attribute::Underline),
            Some(link_target),
        ));
        cells.extend(display_units(")", body_paint, None));
    }
    cells
}

struct NoticeLine {
    end: usize,
    next: usize,
}

fn scan_notice_line(cells: &[Unit<'_>], start: usize, max_width: usize) -> NoticeLine {
    let mut index = start;
    let mut width = 0;
    let mut last_wrap: Option<(usize, usize)> = None;
    while index < cells.len() {
        let cell = &cells[index];
        if cell.text == "\n" || cell.text == "\r" || cell.text == "\r\n" {
            return NoticeLine {
                end: index,
                next: index + 1,
            };
        }
        if cell.text == " " || cell.text == "\t" {
            let next = skip_whitespace(cells, index);
            if width + 1 > max_width {
                return NoticeLine { end: index, next };
            }
            last_wrap = Some((index, next));
            width += 1;
            index += 1;
            continue;
        }
        if width + cell.width > max_width {
            if let Some((end, next)) = last_wrap
                && end > start
            {
                return NoticeLine { end, next };
            }
            if index == start {
                return NoticeLine {
                    end: index + 1,
                    next: index + 1,
                };
            }
            return NoticeLine {
                end: index,
                next: index,
            };
        }
        width += cell.width;
        index += 1;
        if cell.text == "/" || cell.text == "\\" {
            last_wrap = Some((index, index));
        }
    }
    NoticeLine {
        end: index,
        next: index,
    }
}

fn skip_whitespace(cells: &[Unit<'_>], mut index: usize) -> usize {
    while index < cells.len() && (cells[index].text == " " || cells[index].text == "\t") {
        index += 1;
    }
    index
}

fn notice_continuation_indent(cells: &[Unit<'_>], cursor: usize, cols: usize) -> usize {
    if cols <= 2 || cursor >= cells.len() {
        return 0;
    }
    let at_line_start = cursor > 0 && matches!(cells[cursor - 1].text, "\n" | "\r");
    let starts_tree = cells[cursor].text == "├" || cells[cursor].text == "└";
    if at_line_start && starts_tree {
        return 0;
    }
    if cells[cursor].width <= cols - 2 {
        2
    } else {
        0
    }
}

pub(crate) fn render_semantic_notice(notice: &Notice, cols: usize, theme: &Theme) -> Vec<Row> {
    let label = terminal_safe(&notice_label(notice)).into_owned();
    let body = terminal_safe_keeping_breaks(notice.body.trim_end_matches(['\r', '\n']));
    let hyperlink = notice
        .link
        .as_ref()
        .map(|link| (terminal_safe(&link.label), format!(";{}", link.url)));
    let cells = notice_cells(
        notice,
        &label,
        &body,
        hyperlink
            .as_ref()
            .map(|(link_label, link_target)| (link_label.as_ref(), link_target.as_str())),
        theme,
    );
    let mut rows = Vec::new();
    let mut cursor = 0;
    while cursor < cells.len() {
        let indent = if rows.is_empty() {
            0
        } else {
            notice_continuation_indent(&cells, cursor, cols)
        };
        let available = cols.saturating_sub(indent).max(1);
        let line = scan_notice_line(&cells, cursor, available);
        let mut row = Row::new();
        row.push_spaces(indent);
        for cell in &cells[cursor..line.end] {
            row.push_linked(cell.text, cell.paint, cell.link);
        }
        rows.push(row);
        cursor = line.next.max(cursor + 1);
    }
    rows
}

#[cfg(test)]
mod tests {
    use ofx_markdown::MarkdownProcessor;

    use super::*;

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    #[test]
    fn turn_endings_render_as_entries() {
        let theme = theme();
        let summary = Entry::TurnSummary {
            duration_ms: 65_000,
            progress: TokenProgress {
                input_tokens: 1200,
                output_tokens: 34,
            },
        };
        let failure = Entry::TurnFailure {
            text: "⚠ request failed: ConnectionFailed".to_owned(),
        };
        let rendered: Vec<Vec<String>> = [&summary, &Entry::Cancellation, &failure]
            .iter()
            .map(|entry| texts(&entry.render(80, &theme)))
            .collect();
        assert_eq!(
            rendered,
            [
                vec!["  1m 5s (↑1.2k ↓34)"],
                vec!["■ Cancelled · What can oh-fx do differently?"],
                vec!["⚠ request failed: ConnectionFailed"],
            ]
        );
        assert_eq!(failure.render(80, &theme)[0].segments()[0].paint, theme.red);
        for entry in [&summary, &Entry::Cancellation, &failure] {
            assert!(entry.wants_footer_gap());
            assert!(!entry.keeps_trailing_blank());
        }
    }

    #[test]
    fn notices_render_glyph_topic_and_body_with_tone_styles() {
        let notice = Notice::new(NoticeTone::Error, "command", "Unknown command. Try /help.");
        let rows = render_semantic_notice(&notice, 100, &theme());
        assert_eq!(texts(&rows), ["✗ command: Unknown command. Try /help."]);
        assert_eq!(rows[0].segments()[0].paint, Paint::fg(252));
        assert_eq!(rows[0].segments()[0].text, "✗ command:");
        assert_eq!(rows[0].segments()[1].paint, Paint::fg(250));
        let neutral = Notice::new(NoticeTone::Neutral, "", "Switched to other");
        let rows = render_semantic_notice(&neutral, 100, &theme());
        assert_eq!(texts(&rows), ["* Switched to other"]);
        assert_eq!(rows[0].segments().len(), 1);
    }

    #[test]
    fn notice_bodies_cannot_smuggle_terminal_controls() {
        let notice = Notice::new(
            NoticeTone::Neutral,
            "",
            "Switched to mod\x1b]2;PWNED\x07\nsecond line",
        );
        let rows = render_semantic_notice(&notice, 100, &theme());
        assert_eq!(
            texts(&rows),
            ["* Switched to mod\\x1b]2;PWNED\\x07", "  second line"]
        );
        assert!(rows.iter().all(|row| !row.encode().contains('\x07')));
    }

    #[test]
    fn notices_wrap_with_a_two_cell_continuation_indent() {
        let notice = Notice::new(NoticeTone::Neutral, "model", "alpha beta gamma delta");
        let rows = render_semantic_notice(&notice, 16, &theme());
        assert_eq!(texts(&rows), ["* model: alpha", "  beta gamma", "  delta"]);
        let paths = Notice::new(NoticeTone::Neutral, "", "/very/long/path/name");
        assert_eq!(
            texts(&render_semantic_notice(&paths, 12, &theme())),
            ["* /very/", "  long/path/", "  name"]
        );
    }

    #[test]
    fn notices_wrap_at_the_width_of_the_escapes_their_rows_show() {
        let notice = Notice::new(NoticeTone::Neutral, "", "abc\u{202e}defghijkl");
        let rows = render_semantic_notice(&notice, 16, &theme());
        assert!(
            rows.iter().all(|row| row.width() <= 16),
            "{:?}",
            texts(&rows)
        );
        let shown: String = texts(&rows).iter().map(|text| text.trim_start()).collect();
        assert_eq!(shown, "* abc\\u{202e}defghijkl");
    }

    #[test]
    fn notice_links_render_as_an_underlined_hyperlink_label() {
        let notice = Notice::new(NoticeTone::Success, "", "oh-fx has been updated to v1.2.3")
            .with_link("notes", "https://example.com/notes");
        let rows = render_semantic_notice(&notice, 100, &theme());
        assert_eq!(texts(&rows), ["✓ oh-fx has been updated to v1.2.3 (notes)"]);
        let link = rows[0]
            .segments()
            .iter()
            .find(|segment| segment.link.is_some())
            .unwrap();
        assert_eq!(link.text, "notes");
        assert!(link.paint.has(crate::row_text::Attribute::Underline));
    }

    #[test]
    fn turn_summaries_and_cancellations_match_the_upstream_rows() {
        let progress = TokenProgress {
            input_tokens: 4,
            output_tokens: 850,
        };
        assert_eq!(
            turn_summary_row(450, progress, &theme()).text(),
            "  0s (↑4 ↓850)"
        );
        assert_eq!(
            turn_summary_row(125_000, TokenProgress::default(), &theme()).text(),
            "  2m 5s"
        );
        assert_eq!(
            turn_summary_row(3_725_000, progress, &theme()).text(),
            "  1h 02m (↑4 ↓850)"
        );
        let row = cancellation_row(&theme());
        assert_eq!(row.text(), "■ Cancelled · What can oh-fx do differently?");
        assert_eq!(row.segments()[2].paint, Paint::fg(255));
    }

    #[test]
    fn code_panels_wrap_long_lines_under_their_indent() {
        let block = CodeBlockPayload {
            language: String::new(),
            code: "    abcdefghijkl\n".to_owned(),
        };
        let rows = code_panel_rows(&block, 12, &theme());
        assert_eq!(
            texts(&rows),
            [
                "  ──────────",
                "      abcdef",
                "      ghijkl",
                "  ──────────"
            ]
        );
        let narrow = code_panel_rows(&block, 6, &theme());
        assert_eq!(texts(&narrow), ["      ", "  abcd", "  efgh", "  ijkl"]);
    }

    #[test]
    fn code_without_a_language_profile_renders_through_the_text_highlighter() {
        let code = "plain prose with 42 numbers and # no comment\n\x07";
        assert_eq!(infer(code), None);
        let lines = code_lines(code, None, &theme());
        assert_eq!(lines, highlight(code, resolve("text").unwrap(), None));
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.slot.is_none())
        );
        let block = CodeBlockPayload {
            language: String::new(),
            code: code.to_owned(),
        };
        let rows = code_panel_rows(&block, 60, &theme());
        assert!(rows.iter().all(|row| !row.encode().contains('\x07')));
    }

    #[test]
    fn assistant_markdown_renders_inside_the_gutter() {
        let mut processor = MarkdownProcessor::with_completions(ofx_markdown::Completions::ALL);
        let mut events = Vec::new();
        processor.push(
            "Some **bold** text.\n\n- first item\n\n```python\ndef answer():\n    return 42\n```\n\n---\n",
            &mut events,
        );
        processor.flush(&mut events);
        let entry = Entry::Assistant { events };
        let rows = entry.render(30, &theme());
        let texts = texts(&rows);
        assert_eq!(texts[0], "  Some bold text.");
        assert!(texts.contains(&"  • first item".to_owned()));
        assert!(texts.contains(&"  ─ python ────".to_owned()));
        assert!(texts.contains(&"  def answer():".to_owned()));
        assert!(texts.contains(&"      return 42".to_owned()));
        assert!(texts.contains(&"  ─────────────".to_owned()));
        assert_eq!(texts.last().unwrap(), &format!("  {}", "─".repeat(28)));
    }
}
