use std::borrow::Cow;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;

use ofx_contract::{
    ApprovalDecision, ApprovalRequest, FileChangeStats, FileMutation, FileMutationState,
    ProposedFileChange,
};
use ofx_markdown::ReviewOp;
use ofx_text::visible_width;

use super::approval_content::remember_label;
use super::approval_panel::{
    Choice, HINTS, PanelFrame, PanelView, RESIZE_TO_REVIEW, Review, SCREEN_HINTS, SCROLL_TO_REVIEW,
    hint_for,
};
use super::command_text::{
    approval_text, prefix_terminal_safe_by_width, suffix_terminal_safe_by_width,
};
use super::phrase::{PathText, Phrase};
use crate::row_text::{Paint, Row};
use crate::theme::Theme;
use review_document::{DocumentLine, ReviewDocument};

mod review_document;

const INSET: usize = 2;
const RIGHT_MARGIN: usize = 2;
const CHOICE_PREFIX_WIDTH: usize = 7;
const NUMBER_MIN_WIDTH: usize = 5;
const SEPARATOR_ROWS: usize = 2;
const CHECKPOINT_LINES: usize = 64;
const SPACED_MIN_ROWS: usize = 15;
const COMPACT_MIN_ROWS: usize = 8;
const ELLIPSIS: &str = "…";
const STATS_SEPARATOR: &str = " · ";
const QUESTION_SEPARATOR: &str = "  ·  ";
const SOLID_DIVIDER: &str = "─";
const DOTTED_DIVIDER: &str = "┄";
const SELECTED_MARKER: &str = "❯ ";
const BLOCKED_MARKER: &str = "! ";
const UNREAD_NOTICE: &str = "This file is read only once the change is approved, so the change cannot be shown before then.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Mutation,
    Disclosure,
}

impl Intent {
    fn title(self) -> &'static str {
        match self {
            Self::Mutation => "Permission needed · Review change",
            Self::Disclosure => "Permission needed · Review file",
        }
    }

    fn question(self) -> &'static str {
        match self {
            Self::Mutation => "Apply this change?",
            Self::Disclosure => "Reveal that this file already matches?",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Mutation => "Apply",
            Self::Disclosure => "Reveal",
        }
    }

    fn denial(self) -> &'static str {
        match self {
            Self::Mutation => "Don't apply",
            Self::Disclosure => "Don't reveal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileApproval {
    intent: Intent,
    action: &'static str,
    path: PathText,
    review: ReviewDocument,
    remember: Option<Phrase>,
}

impl FileApproval {
    pub(crate) fn new(
        request: &ApprovalRequest,
        file: &FileMutation,
        change: Option<&ProposedFileChange>,
    ) -> Self {
        let intent = if file.state == FileMutationState::Unchanged {
            Intent::Disclosure
        } else {
            Intent::Mutation
        };
        let action = match (intent, request.tool_name.as_str()) {
            (Intent::Disclosure, _) => "Check",
            (Intent::Mutation, "edit_file") => "Edit",
            (Intent::Mutation, _) => "Write",
        };
        let (path, review) = match change {
            Some(change) => (
                PathText::from_encoded(&change.display_path),
                ReviewDocument::review(change.before.as_deref().unwrap_or_default(), &change.after),
            ),
            None => (
                PathText::from_raw(file.target.as_os_str().as_bytes()),
                ReviewDocument::notice(UNREAD_NOTICE),
            ),
        };
        Self {
            intent,
            action,
            path,
            review,
            remember: request.scope.always.as_ref().map(remember_label),
        }
    }

    pub(crate) fn choices(&self) -> Vec<Choice> {
        let verb = self.intent.verb();
        let mut choices = vec![Choice::new(
            b'1',
            Phrase::plain(format!("{verb} once")),
            ApprovalDecision::Once,
        )];
        if let Some(remember) = &self.remember {
            choices.push(Choice::new(
                b'2',
                remember.clone().after(&format!("{verb} + ")),
                ApprovalDecision::Always,
            ));
        }
        choices.push(Choice::new(
            b'3',
            Phrase::plain(self.intent.denial()),
            ApprovalDecision::Deny,
        ));
        choices
    }

    fn lines(&self, start: usize, count: usize) -> impl Iterator<Item = DocumentLine<'_>> {
        self.review.lines_from(start).take(count)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewLayout {
    cols: usize,
    checkpoints: Vec<usize>,
    rows: usize,
    drawable: bool,
}

impl ReviewLayout {
    pub(crate) fn measure(file: &FileApproval, cols: usize) -> Self {
        let count = file.review.len();
        let mut checkpoints = Vec::with_capacity(count.div_ceil(CHECKPOINT_LINES));
        let mut rows = 0;
        let mut drawable = true;
        for chunk in (0..count).step_by(CHECKPOINT_LINES) {
            checkpoints.push(rows);
            for line in file.lines(chunk, CHECKPOINT_LINES) {
                let (line_rows, fits) = line_rows(&line, cols);
                rows += line_rows;
                drawable &= fits;
            }
        }
        Self {
            cols,
            checkpoints,
            rows,
            drawable,
        }
    }

    pub(crate) fn cols(&self) -> usize {
        self.cols
    }

    fn rows(&self) -> usize {
        self.rows
    }
}

fn line_rows(line: &DocumentLine<'_>, cols: usize) -> (usize, bool) {
    if let Some(rows) = plain_row_count(line, cols) {
        return (rows, true);
    }
    let mut rows = 0;
    let fits = for_each_segment(line, &line_text(line), cols, |_, _| rows += 1);
    (rows, fits)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chrome {
    Spacer,
    DottedDivider,
    Divider,
    Header,
    Question,
    Choice(usize),
    Hint,
}

fn chrome(screen_rows: usize, choice_count: usize) -> Vec<Chrome> {
    let choices = (0..choice_count).map(Chrome::Choice);
    if screen_rows >= SPACED_MIN_ROWS {
        [
            Chrome::DottedDivider,
            Chrome::Header,
            Chrome::Spacer,
            Chrome::Question,
            Chrome::Spacer,
        ]
        .into_iter()
        .chain(choices)
        .chain([Chrome::Spacer, Chrome::Divider, Chrome::Hint])
        .collect()
    } else if screen_rows >= COMPACT_MIN_ROWS {
        [Chrome::DottedDivider, Chrome::Header, Chrome::Question]
            .into_iter()
            .chain(choices)
            .chain([Chrome::Divider, Chrome::Hint])
            .collect()
    } else {
        choices.take(screen_rows).collect()
    }
}

struct Controls<'a> {
    file: &'a FileApproval,
    header: Header,
    choices: &'a [Choice],
    selected: usize,
    blocked: Option<&'static str>,
    scrollable: bool,
    cols: usize,
}

pub(crate) fn file_approval_rows(
    theme: &Theme,
    file: &FileApproval,
    layout: &ReviewLayout,
    choices: &[Choice],
    selected: usize,
    frame: PanelFrame<'_>,
    change_seen: bool,
) -> PanelView {
    let cols = frame.cols;
    let header = project_header(file, cols);
    let labels_fit = cols >= CHOICE_PREFIX_WIDTH
        && choices
            .iter()
            .all(|choice| choice.label.fit(cols - CHOICE_PREFIX_WIDTH).1);
    let drawable = layout.drawable && header.complete() && labels_fit;
    let review_rows = layout.rows();
    let kinds = chrome(frame.screen_rows, choices.len());
    let full_chrome = kinds.len() > choices.len();
    let inline = full_chrome && SEPARATOR_ROWS + review_rows + kinds.len() <= frame.inline_rows;
    let mut rows = Vec::new();
    let window = if inline {
        rows.push(Row::new());
        rows.push(divider(theme, SOLID_DIVIDER, cols));
        0..review_rows
    } else {
        let window_rows = if full_chrome {
            frame.screen_rows - kinds.len()
        } else {
            0
        };
        let scroll = frame.scroll.min(review_rows.saturating_sub(window_rows));
        scroll..(scroll + window_rows).min(review_rows)
    };
    let review_start = rows.len();
    let (drawn, change_shown) = review_window(theme, file, layout, &window);
    rows.extend(drawn);
    let complete = drawable && !window.is_empty();
    let blocked = if !complete {
        Some(RESIZE_TO_REVIEW)
    } else if change_shown || change_seen {
        None
    } else {
        Some(SCROLL_TO_REVIEW)
    };
    let controls = Controls {
        file,
        header,
        choices,
        selected,
        blocked,
        scrollable: window.len() < review_rows,
        cols,
    };
    let mut choices_end = rows.len();
    for kind in kinds {
        rows.push(controls.row(theme, kind));
        if matches!(kind, Chrome::Choice(_)) {
            choices_end = rows.len();
        }
    }
    PanelView {
        rows: rows.into_iter().map(|row| row.clipped(cols)).collect(),
        review: Review {
            required_rows: review_start..choices_end,
            window,
            action_rows: review_rows,
            complete,
            screen: !inline,
            change_shown: Some(change_shown),
        },
    }
}

fn review_window(
    theme: &Theme,
    file: &FileApproval,
    layout: &ReviewLayout,
    window: &Range<usize>,
) -> (Vec<Row>, bool) {
    if window.is_empty() {
        return (Vec::new(), false);
    }
    let checkpoint = layout
        .checkpoints
        .partition_point(|start| *start <= window.start)
        .saturating_sub(1);
    let mut visual = layout.checkpoints[checkpoint];
    let mut rows = Vec::with_capacity(window.len());
    let mut change_shown = false;
    for line in file.lines(
        checkpoint * CHECKPOINT_LINES,
        CHECKPOINT_LINES + window.len(),
    ) {
        if visual >= window.end {
            break;
        }
        let skipped = plain_row_count(&line, layout.cols)
            .filter(|line_rows| visual + line_rows <= window.start);
        if let Some(line_rows) = skipped {
            visual += line_rows;
            continue;
        }
        let text = line_text(&line);
        for_each_segment(&line, &text, layout.cols, |segment, first_segment| {
            if window.contains(&visual) {
                rows.push(review_row(theme, &line, segment, first_segment));
                change_shown |= shows_change(line.op);
            }
            visual += 1;
        });
    }
    (rows, change_shown)
}

fn shows_change(op: ReviewOp) -> bool {
    match op {
        ReviewOp::Addition | ReviewOp::Deletion | ReviewOp::Notice => true,
        ReviewOp::Context | ReviewOp::Elision => false,
    }
}

fn plain_row_count(line: &DocumentLine<'_>, cols: usize) -> Option<usize> {
    if line.op == ReviewOp::Elision || line.text.contains(&b'\\') || printable(line).is_none() {
        return None;
    }
    let width = line.text.len();
    let first = cols.saturating_sub(prefix_width(line, true));
    let continuation = cols.saturating_sub(prefix_width(line, false));
    if width <= first {
        return Some(1);
    }
    if first == 0 || continuation == 0 {
        return None;
    }
    Some(1 + (width - first).div_ceil(continuation))
}

fn line_text<'a>(line: &DocumentLine<'a>) -> Cow<'a, str> {
    if line.op == ReviewOp::Elision {
        Cow::Owned(format!("{} unchanged lines ⋯", line.label))
    } else {
        printable(line).map_or_else(|| Cow::Owned(approval_text(line.text)), Cow::Borrowed)
    }
}

fn printable<'a>(line: &DocumentLine<'a>) -> Option<&'a str> {
    line.text
        .iter()
        .all(|byte| (b' '..=b'~').contains(byte))
        .then(|| std::str::from_utf8(line.text).ok())
        .flatten()
}

fn for_each_segment<'a>(
    line: &DocumentLine<'_>,
    text: &'a str,
    cols: usize,
    mut visit: impl FnMut(&'a str, bool),
) -> bool {
    let mut offset = 0;
    let mut first = true;
    while first || offset < text.len() {
        let available = cols.saturating_sub(prefix_width(line, first));
        let segment = prefix_terminal_safe_by_width(&text[offset..], available);
        if offset < text.len() && segment.is_empty() {
            return false;
        }
        visit(segment, first);
        offset += segment.len();
        first = false;
    }
    true
}

fn prefix_width(line: &DocumentLine<'_>, first: bool) -> usize {
    match (first, line.op) {
        (false, _) => NUMBER_MIN_WIDTH + 5,
        (true, ReviewOp::Notice) => NUMBER_MIN_WIDTH + 4,
        (true, _) => number_width(line) + 5,
    }
}

fn number_width(line: &DocumentLine<'_>) -> usize {
    line.number().map_or(NUMBER_MIN_WIDTH, |number| {
        decimal_digits(number as u64).max(NUMBER_MIN_WIDTH)
    })
}

fn review_row(theme: &Theme, line: &DocumentLine<'_>, segment: &str, first: bool) -> Row {
    let text_paint = review_paint(theme, line.op);
    let mut row = Row::new();
    match (first, line.op) {
        (false, _) => row.push_spaces(NUMBER_MIN_WIDTH + 5),
        (true, ReviewOp::Notice) => row.push_spaces(INSET + NUMBER_MIN_WIDTH + 2),
        (true, op) => {
            let number = line
                .number()
                .map(|number| number.to_string())
                .unwrap_or_default();
            let marker = marker_paint(theme, op);
            row.push_spaces(INSET + number_width(line) - number.len());
            row.push(&number, marker.unwrap_or(theme.statusline));
            row.push(" ", Paint::PLAIN);
            row.push(sign(op), marker.unwrap_or(text_paint));
            row.push(" ", Paint::PLAIN);
        }
    }
    row.push(segment, text_paint);
    row
}

fn review_paint(theme: &Theme, op: ReviewOp) -> Paint {
    match op {
        ReviewOp::Addition => theme.green,
        ReviewOp::Deletion => theme.red,
        ReviewOp::Context | ReviewOp::Elision | ReviewOp::Notice => theme.dim,
    }
}

fn marker_paint(theme: &Theme, op: ReviewOp) -> Option<Paint> {
    let (added, removed) = theme.diff_marker_paints()?;
    match op {
        ReviewOp::Addition => Some(added),
        ReviewOp::Deletion => Some(removed),
        ReviewOp::Context | ReviewOp::Elision | ReviewOp::Notice => None,
    }
}

fn sign(op: ReviewOp) -> &'static str {
    match op {
        ReviewOp::Addition => "+",
        ReviewOp::Deletion => "-",
        ReviewOp::Elision => "⋯",
        ReviewOp::Context | ReviewOp::Notice => " ",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PathProjection {
    start: usize,
    ellipsis: bool,
    complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    path: PathProjection,
    stats: Option<FileChangeStats>,
    action_visible: bool,
}

impl Header {
    fn complete(self) -> bool {
        self.action_visible && self.path.complete
    }
}

fn project_header(file: &FileApproval, cols: usize) -> Header {
    let action_width = visible_width(file.action);
    let inner = cols.saturating_sub(INSET + RIGHT_MARGIN);
    let action_visible = action_width <= inner;
    let mut header = Header {
        path: PathProjection {
            start: 0,
            ellipsis: false,
            complete: false,
        },
        stats: None,
        action_visible,
    };
    if inner <= action_width {
        return header;
    }
    let changes = file.review.stats();
    if changes != FileChangeStats::from_lines(0, 0) {
        let fixed = action_width + 1 + 2 + stats_width(changes);
        if fixed <= inner {
            let path = project_path(&file.path, inner - fixed);
            if path.complete {
                header.path = path;
                header.stats = Some(changes);
                return header;
            }
        }
    }
    header.path = project_path(&file.path, inner - action_width - 1);
    header
}

fn project_path(path: &PathText, width: usize) -> PathProjection {
    let text = path.text();
    let hidden = PathProjection {
        start: text.len(),
        ellipsis: false,
        complete: false,
    };
    if width == 0 {
        return hidden;
    }
    if visible_width(text) <= width {
        return PathProjection {
            start: 0,
            ellipsis: false,
            complete: true,
        };
    }
    let ellipsis_width = visible_width(ELLIPSIS);
    if width <= ellipsis_width {
        return hidden;
    }
    let start = text.len() - suffix_terminal_safe_by_width(text, width - ellipsis_width).len();
    PathProjection {
        start,
        ellipsis: true,
        complete: start <= path.basename_start(),
    }
}

fn stats_width(changes: FileChangeStats) -> usize {
    4 + decimal_digits(changes.additions.into()) + decimal_digits(changes.deletions.into())
}

fn decimal_digits(value: u64) -> usize {
    value
        .checked_ilog10()
        .map_or(1, |digits| digits as usize + 1)
}

impl Controls<'_> {
    fn row(&self, theme: &Theme, kind: Chrome) -> Row {
        match kind {
            Chrome::Spacer => Row::new(),
            Chrome::DottedDivider => divider(theme, DOTTED_DIVIDER, self.cols),
            Chrome::Divider => divider(theme, SOLID_DIVIDER, self.cols),
            Chrome::Header => self.header_row(theme),
            Chrome::Question => self.question_row(),
            Chrome::Choice(index) => self.choice_row(theme, index),
            Chrome::Hint => self.hint_row(theme),
        }
    }

    fn header_row(&self, theme: &Theme) -> Row {
        let title = self.file.intent.title();
        let inner = self.cols.saturating_sub(INSET + RIGHT_MARGIN);
        let mut row = Row::new();
        row.push_spaces(INSET.min(self.cols));
        let (shown, whole) = clip(title, inner);
        row.push(&shown, Paint::PLAIN.with_bold());
        let action_width = visible_width(self.file.action);
        let right_width = action_width
            + self.header.stats.map_or(0, |changes| {
                visible_width(STATS_SEPARATOR) + stats_width(changes)
            });
        let title_width = visible_width(title);
        if !whole || title_width + right_width + 1 > inner {
            return row;
        }
        row.push_spaces(inner - title_width - right_width);
        row.push(self.file.action, theme.statusline);
        if let Some(changes) = self.header.stats {
            row.push(STATS_SEPARATOR, Paint::PLAIN);
            row.push(&format!("+{}", changes.additions), theme.green);
            row.push("  ", Paint::PLAIN);
            row.push(&format!("-{}", changes.deletions), theme.red);
        }
        row
    }

    fn question_row(&self) -> Row {
        let mut remaining = self.cols;
        let mut row = Row::new();
        push_spaces_within(&mut row, INSET, &mut remaining);
        let path = self.file.path.text();
        let projection = self.header.path;
        if projection.start < path.len() && remaining > 0 {
            let bold = Paint::PLAIN.with_bold();
            if projection.ellipsis {
                row.push(&clip(ELLIPSIS, remaining).0, bold);
                remaining = remaining.saturating_sub(visible_width(ELLIPSIS));
            }
            let tail = &path[projection.start..];
            row.push(&clip(tail, remaining).0, bold);
            remaining -= visible_width(tail).min(remaining);
        }
        if remaining > 0 {
            row.push(&clip(QUESTION_SEPARATOR, remaining).0, Paint::PLAIN);
            remaining -= visible_width(QUESTION_SEPARATOR).min(remaining);
        }
        row.push(
            &clip(self.file.intent.question(), remaining).0,
            Paint::PLAIN,
        );
        row
    }

    fn choice_row(&self, theme: &Theme, index: usize) -> Row {
        let choice = &self.choices[index];
        let selected = index == self.selected;
        let blocked = self
            .blocked
            .filter(|_| choice.decision != ApprovalDecision::Deny);
        let paint = if blocked.is_some() {
            theme.statusline
        } else if selected {
            theme.tag
        } else {
            Paint::PLAIN
        };
        let mut remaining = self.cols;
        let mut row = Row::new();
        push_spaces_within(&mut row, INSET, &mut remaining);
        let marker = if selected { SELECTED_MARKER } else { "  " };
        push_within(&mut row, marker, paint, &mut remaining);
        if blocked.is_some() {
            push_within(&mut row, BLOCKED_MARKER, paint, &mut remaining);
        }
        let number_paint = if blocked.is_none() && !selected {
            theme.statusline
        } else {
            paint
        };
        push_within(
            &mut row,
            &char::from(choice.key).to_string(),
            number_paint,
            &mut remaining,
        );
        push_within(&mut row, "  ", paint, &mut remaining);
        let (label, _) = choice.label.fit(remaining);
        push_within(&mut row, &label, paint, &mut remaining);
        if let Some(reason) = blocked.filter(|_| selected) {
            push_within(&mut row, reason, paint, &mut remaining);
        }
        row
    }

    fn hint_row(&self, theme: &Theme) -> Row {
        let hints: &[&'static str] = if self.scrollable {
            &SCREEN_HINTS
        } else {
            &HINTS
        };
        let width = self.cols.saturating_sub(INSET);
        let mut row = Row::new();
        row.push_spaces(INSET.min(self.cols));
        row.push(&clip(hint_for(hints, width), width).0, theme.statusline);
        row
    }
}

fn divider(theme: &Theme, glyph: &str, cols: usize) -> Row {
    Row::styled(&glyph.repeat(cols), theme.divider)
}

fn push_spaces_within(row: &mut Row, count: usize, remaining: &mut usize) {
    let taken = count.min(*remaining);
    row.push_spaces(taken);
    *remaining -= taken;
}

fn push_within(row: &mut Row, text: &str, paint: Paint, remaining: &mut usize) {
    let (shown, _) = clip(text, *remaining);
    row.push(&shown, paint);
    *remaining -= visible_width(&shown).min(*remaining);
}

fn clip(text: &str, width: usize) -> (String, bool) {
    if visible_width(text) <= width {
        return (text.to_owned(), true);
    }
    let ellipsis_width = visible_width(ELLIPSIS);
    if width == 0 {
        return (String::new(), false);
    }
    if width <= ellipsis_width {
        return (ELLIPSIS.to_owned(), false);
    }
    let prefix = prefix_terminal_safe_by_width(text, width - ellipsis_width);
    (format!("{prefix}{ELLIPSIS}"), false)
}

#[cfg(test)]
mod tests;
