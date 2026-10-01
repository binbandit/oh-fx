use ofx_contract::ApprovalDecision;
use ofx_text::visible_width;

use crate::row_text::{Paint, Row};
use crate::theme::Theme;

const HEADER: &str = "Permission needed · Choose one";
const KIND: &str = "Tool";
const QUESTION: &str = "Would you like to allow this action?";
const REASON: &str = " This action needs approval before oh-fx can continue.";
const REMEMBERING_CHOICES: [Choice; 3] = [
    Choice::new(b'1', "1. Yes", ApprovalDecision::Once),
    Choice::new(
        b'2',
        "2. Yes, and don't ask again for this request",
        ApprovalDecision::Always,
    ),
    Choice::new(b'3', "3. No", ApprovalDecision::Deny),
];
const ONE_TIME_CHOICES: [Choice; 2] = [
    Choice::new(b'1', "1. Yes", ApprovalDecision::Once),
    Choice::new(b'3', "3. No", ApprovalDecision::Deny),
];
const HINTS: [&str; 3] = [
    "1–3 choose now    ↑↓ or tab options    enter confirm    esc cancel",
    "1–3 choose now    enter confirm    esc cancel",
    "enter confirm    esc cancel",
];
const INSET: usize = 2;
const SPACIOUS_MIN_TERMINAL_ROWS: u16 = 34;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) key: u8,
    label: &'static str,
    pub(crate) decision: ApprovalDecision,
}

impl Choice {
    const fn new(key: u8, label: &'static str, decision: ApprovalDecision) -> Self {
        Self {
            key,
            label,
            decision,
        }
    }
}

pub(crate) fn choices(remembers: bool) -> &'static [Choice] {
    if remembers {
        &REMEMBERING_CHOICES
    } else {
        &ONE_TIME_CHOICES
    }
}

pub(crate) fn approval_panel_rows(
    theme: &Theme,
    action: &str,
    choices: &[Choice],
    selected: usize,
    cols: usize,
    terminal_rows: u16,
) -> Vec<Row> {
    let spacious = terminal_rows >= SPACIOUS_MIN_TERMINAL_ROWS;
    let mut rows = vec![header_row(theme, cols)];
    if spacious {
        rows.push(Row::new());
    }
    rows.push(inset(QUESTION, Paint::PLAIN.with_bold()));
    let mut reason = inset("Reason:", theme.dim);
    reason.push(REASON, Paint::PLAIN);
    rows.push(reason);
    rows.push(inset(action, Paint::PLAIN));
    if spacious {
        rows.push(Row::new());
    }
    for (index, choice) in choices.iter().enumerate() {
        rows.push(choice_row(theme, choice.label, index == selected));
    }
    if spacious {
        rows.push(Row::new());
    }
    rows.push(inset(hint_for(cols.saturating_sub(INSET)), theme.dim));
    rows.into_iter().map(|row| row.clipped(cols)).collect()
}

fn inset(text: &str, paint: Paint) -> Row {
    let mut row = Row::new();
    row.push_spaces(INSET);
    row.push(text, paint);
    row
}

fn header_row(theme: &Theme, cols: usize) -> Row {
    let inner = cols.saturating_sub(INSET * 2);
    let mut row = inset(HEADER, Paint::PLAIN.with_bold()).clipped(INSET + inner);
    let title_width = visible_width(HEADER);
    let kind_width = visible_width(KIND);
    if title_width + kind_width < inner {
        row.push_spaces(inner - title_width - kind_width);
        row.push(KIND, theme.statusline);
    }
    row
}

fn choice_row(theme: &Theme, label: &str, selected: bool) -> Row {
    let mut row = Row::new();
    row.push_spaces(INSET);
    if selected {
        row.push("❯ ", Paint::PLAIN);
        row.push(label, theme.tag);
    } else {
        row.push_spaces(2);
        row.push(label, Paint::PLAIN);
    }
    row
}

fn hint_for(width: usize) -> &'static str {
    HINTS
        .iter()
        .find(|hint| visible_width(hint) <= width)
        .unwrap_or(&HINTS[HINTS.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    #[test]
    fn compact_panels_follow_upstreams_generic_approval_rows() {
        let rows = approval_panel_rows(&theme(), "Reading ../notes.txt", choices(true), 0, 80, 24);
        let header = format!("  {HEADER}{}Tool", " ".repeat(76 - 30 - 4));
        assert_eq!(
            texts(&rows),
            [
                header.as_str(),
                "  Would you like to allow this action?",
                "  Reason: This action needs approval before oh-fx can continue.",
                "  Reading ../notes.txt",
                "  ❯ 1. Yes",
                "    2. Yes, and don't ask again for this request",
                "    3. No",
                "  1–3 choose now    ↑↓ or tab options    enter confirm    esc cancel",
            ]
        );
        assert_eq!(rows[4].segments()[1].paint, theme().tag);
        assert_eq!(rows[0].segments().last().unwrap().paint, theme().statusline);
        assert_eq!(rows[2].segments()[1].paint, theme().dim);
    }

    #[test]
    fn tall_terminals_space_the_panel_out() {
        let rows = approval_panel_rows(&theme(), "Reading a", choices(true), 2, 80, 34);
        let texts = texts(&rows);
        assert_eq!(texts.len(), 11);
        assert_eq!(texts[1], "");
        assert_eq!(texts[5], "");
        assert_eq!(texts[8], "  ❯ 3. No");
        assert_eq!(texts[9], "");
    }

    #[test]
    fn narrow_panels_drop_the_kind_and_keep_the_confirm_and_cancel_hints() {
        let rows = approval_panel_rows(&theme(), "Reading a", choices(true), 1, 34, 24);
        let texts = texts(&rows);
        assert_eq!(texts[0], "  Permission needed · Choose one");
        assert_eq!(texts[5], "  ❯ 2. Yes, and don't ask again fo");
        assert_eq!(texts[7], "  enter confirm    esc cancel");
        assert!(rows.iter().all(|row| row.width() <= 34));
        assert_eq!(
            texts_at(50)[7],
            "  1–3 choose now    enter confirm    esc cancel"
        );
    }

    fn texts_at(cols: usize) -> Vec<String> {
        texts(&approval_panel_rows(
            &theme(),
            "Reading a",
            choices(true),
            0,
            cols,
            24,
        ))
    }

    #[test]
    fn choices_map_to_upstreams_decisions() {
        let decisions = |remembers| {
            choices(remembers)
                .iter()
                .map(|choice| (choice.key, choice.decision))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            decisions(true),
            [
                (b'1', ApprovalDecision::Once),
                (b'2', ApprovalDecision::Always),
                (b'3', ApprovalDecision::Deny)
            ]
        );
        assert_eq!(
            decisions(false),
            [
                (b'1', ApprovalDecision::Once),
                (b'3', ApprovalDecision::Deny)
            ]
        );
    }

    #[test]
    fn requests_that_remember_nothing_offer_only_yes_and_no() {
        let rows = approval_panel_rows(&theme(), "Reading a", choices(false), 1, 80, 24);
        let texts = texts(&rows);
        assert_eq!(
            &texts[4..],
            [
                "    1. Yes",
                "  ❯ 3. No",
                "  1–3 choose now    ↑↓ or tab options    enter confirm    esc cancel"
            ]
        );
    }
}
