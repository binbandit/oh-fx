use std::ops::Range;

use ofx_contract::ApprovalDecision;
use ofx_text::visible_width;

use super::approval_content::{ActionBlock, ApprovalContent};
use super::command_text::command_segments;
use super::phrase::Phrase;
use crate::row_text::{Paint, Row};
use crate::theme::Theme;

const HEADER: &str = "Permission needed · Choose one";
const REASON_LABEL: &str = "Reason:";
const HINTS: [&str; 3] = [
    "1–3 choose now    ↑↓ or tab options    enter confirm    esc cancel",
    "1–3 choose now    enter confirm    esc cancel",
    "enter confirm    esc cancel",
];
const INSET: usize = 2;
const CHOICE_MARKER_WIDTH: usize = 2;
const SPACIOUS_MIN_TERMINAL_ROWS: u16 = 34;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) key: u8,
    label: Phrase,
    pub(crate) decision: ApprovalDecision,
}

impl Choice {
    fn new(key: u8, label: Phrase, decision: ApprovalDecision) -> Self {
        Self {
            key,
            label,
            decision,
        }
    }
}

pub(crate) fn choices(remember: Option<&Phrase>) -> Vec<Choice> {
    let yes = Choice::new(b'1', Phrase::plain("1. Yes"), ApprovalDecision::Once);
    let no = Choice::new(b'3', Phrase::plain("3. No"), ApprovalDecision::Deny);
    match remember {
        Some(remember) => vec![
            yes,
            Choice::new(
                b'2',
                remember.clone().after("2. Yes, and "),
                ApprovalDecision::Always,
            ),
            no,
        ],
        None => vec![yes, no],
    }
}

pub(crate) struct PanelView {
    pub(crate) rows: Vec<Row>,
    pub(crate) required_rows: Option<Range<usize>>,
}

pub(crate) fn approval_panel_rows(
    theme: &Theme,
    content: &ApprovalContent,
    choices: &[Choice],
    selected: usize,
    cols: usize,
    terminal_rows: u16,
) -> PanelView {
    let spacious = terminal_rows >= SPACIOUS_MIN_TERMINAL_ROWS;
    let mut rows = vec![header_row(theme, content.kind, cols)];
    if spacious {
        rows.push(Row::new());
    }
    rows.push(inset(content.question, Paint::PLAIN.with_bold()));
    rows.push(reason_row(theme, content.reason.as_deref()));
    let action_start = rows.len();
    let mut complete = true;
    for block in &content.action {
        let (block_rows, block_complete) = action_rows(theme, block, cols);
        rows.extend(block_rows);
        complete &= block_complete;
    }
    if spacious {
        rows.push(Row::new());
    }
    for (index, choice) in choices.iter().enumerate() {
        let (label, fits) = choice
            .label
            .fit(cols.saturating_sub(INSET + CHOICE_MARKER_WIDTH));
        complete &= fits;
        rows.push(choice_row(theme, &label, index == selected));
    }
    let required_rows = complete.then_some(action_start..rows.len());
    if spacious {
        rows.push(Row::new());
    }
    rows.push(inset(hint_for(cols.saturating_sub(INSET)), theme.dim));
    PanelView {
        rows: rows.into_iter().map(|row| row.clipped(cols)).collect(),
        required_rows,
    }
}

fn inset(text: &str, paint: Paint) -> Row {
    let mut row = Row::new();
    row.push_spaces(INSET);
    row.push(text, paint);
    row
}

fn header_row(theme: &Theme, kind: &str, cols: usize) -> Row {
    let inner = cols.saturating_sub(INSET * 2);
    let mut row = inset(HEADER, Paint::PLAIN.with_bold()).clipped(INSET + inner);
    let title_width = visible_width(HEADER);
    let kind_width = visible_width(kind);
    if title_width + kind_width < inner {
        row.push_spaces(inner - title_width - kind_width);
        row.push(kind, theme.statusline);
    }
    row
}

fn reason_row(theme: &Theme, reason: Option<&str>) -> Row {
    let Some(reason) = reason else {
        return Row::new();
    };
    let mut row = inset(REASON_LABEL, theme.dim);
    row.push(" ", Paint::PLAIN);
    row.push(reason, Paint::PLAIN);
    row
}

fn action_rows(theme: &Theme, block: &ActionBlock, cols: usize) -> (Vec<Row>, bool) {
    match block {
        ActionBlock::Line(phrase) => {
            let (text, complete) = phrase.fit(cols.saturating_sub(INSET));
            (vec![inset(&text, Paint::PLAIN)], complete)
        }
        ActionBlock::Note(note) => {
            let row = inset(note, theme.dim);
            let complete = row.width() <= cols;
            (vec![row], complete)
        }
        ActionBlock::Wrapped { lead, text } => {
            let lead_width = visible_width(lead);
            let continuation = " ".repeat(lead_width);
            let content_width = cols.saturating_sub(INSET + lead_width);
            let segments = command_segments(text, content_width);
            let complete = segments.is_some();
            let rows = segments
                .unwrap_or_else(|| vec![text])
                .iter()
                .enumerate()
                .map(|(index, segment)| {
                    let mut row = Row::new();
                    row.push_spaces(INSET);
                    row.push(if index == 0 { lead } else { &continuation }, theme.tag);
                    row.push(segment, theme.tag);
                    row
                })
                .collect();
            (rows, complete)
        }
    }
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
    use std::path::Path;

    use ofx_contract::{
        ApprovalRequest, ApprovalScope, CommandProfile, CommandRequest, PathAccess, RequestId,
        SessionGrant,
    };

    use super::*;

    fn remember() -> Phrase {
        Phrase::plain("don't ask again for this request")
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    fn titled(title: &str) -> ApprovalContent {
        ApprovalContent {
            kind: "Tool",
            question: "Would you like to allow this action?",
            reason: Some("This action needs approval before oh-fx can continue.".to_owned()),
            action: vec![ActionBlock::Line(Phrase::plain(title))],
            remember: None,
        }
    }

    fn rows(title: &str, remember: Option<&Phrase>, selected: usize, cols: usize) -> Vec<Row> {
        approval_panel_rows(
            &theme(),
            &titled(title),
            &choices(remember),
            selected,
            cols,
            24,
        )
        .rows
    }

    fn command_content(command: &str) -> ApprovalContent {
        let request = ApprovalRequest {
            id: RequestId::new(1),
            tool_name: "shell".to_owned(),
            title: format!("Running {}...", &command[..20]),
            tool_arguments_preview: String::new(),
            scope: ApprovalScope {
                target: None,
                access: PathAccess::WorkspaceOnly,
                always: Some(SessionGrant::Command {
                    command: command.to_owned(),
                    profile: CommandProfile::User,
                    shell: None,
                    terminal: false,
                }),
            },
            command: Some(CommandRequest::Run {
                command: command.to_owned(),
                cwd: Path::new("/ws").to_path_buf(),
                profile: CommandProfile::User,
                shell: None,
                terminal: false,
            }),
            file: None,
        };
        ApprovalContent::from_request(&request, Path::new("/ws"))
    }

    #[test]
    fn compact_panels_follow_upstreams_generic_approval_rows() {
        let rows = rows("Reading ../notes.txt", Some(&remember()), 0, 80);
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
        let rows = approval_panel_rows(
            &theme(),
            &titled("Reading a"),
            &choices(Some(&remember())),
            2,
            80,
            34,
        )
        .rows;
        let texts = texts(&rows);
        assert_eq!(texts.len(), 11);
        assert_eq!(texts[1], "");
        assert_eq!(texts[5], "");
        assert_eq!(texts[8], "  ❯ 3. No");
        assert_eq!(texts[9], "");
    }

    #[test]
    fn narrow_panels_drop_the_kind_and_keep_the_confirm_and_cancel_hints() {
        let narrow = rows("Reading a", Some(&remember()), 1, 34);
        let shown = texts(&narrow);
        assert_eq!(shown[0], "  Permission needed · Choose one");
        assert_eq!(shown[5], "  ❯ 2. Yes, and don't ask again fo");
        assert_eq!(shown[7], "  enter confirm    esc cancel");
        assert!(narrow.iter().all(|row| row.width() <= 34));
        assert_eq!(
            texts(&rows("Reading a", Some(&remember()), 0, 50))[7],
            "  1–3 choose now    enter confirm    esc cancel"
        );
    }

    #[test]
    fn panels_name_the_rows_that_must_be_seen_only_when_nothing_is_cut() {
        let view = |title: &str, cols| {
            approval_panel_rows(
                &theme(),
                &titled(title),
                &choices(Some(&remember())),
                0,
                cols,
                34,
            )
            .required_rows
        };
        assert_eq!(view("Reading a", 80), Some(4..9));
        assert_eq!(view("Reading a", 40), None);
        assert_eq!(view(&"a".repeat(79), 80), None);
    }

    #[test]
    fn choices_map_to_upstreams_decisions() {
        let decisions = |remember: Option<&Phrase>| {
            choices(remember)
                .iter()
                .map(|choice| (choice.key, choice.decision))
                .collect::<Vec<_>>()
        };
        let remember = remember();
        assert_eq!(
            decisions(Some(&remember)),
            [
                (b'1', ApprovalDecision::Once),
                (b'2', ApprovalDecision::Always),
                (b'3', ApprovalDecision::Deny)
            ]
        );
        assert_eq!(
            decisions(None),
            [
                (b'1', ApprovalDecision::Once),
                (b'3', ApprovalDecision::Deny)
            ]
        );
    }

    #[test]
    fn requests_that_remember_nothing_offer_only_yes_and_no() {
        let texts = texts(&rows("Reading a", None, 1, 80));
        assert_eq!(
            &texts[4..],
            [
                "    1. Yes",
                "  ❯ 3. No",
                "  1–3 choose now    ↑↓ or tab options    enter confirm    esc cancel"
            ]
        );
    }

    #[test]
    fn inline_command_panel_never_truncates_the_complete_command() {
        let command = format!(
            "echo {} && touch ../PWNED_BY_HIDDEN_TAIL",
            ["building-the-project-please-wait"; 3].join(" ")
        );
        let content = command_content(&command);
        let rows = approval_panel_rows(
            &theme(),
            &content,
            &choices(content.remember.as_ref()),
            0,
            100,
            24,
        );
        assert_eq!(rows.required_rows, Some(3..8));
        let rows = rows.rows;
        let texts = texts(&rows);
        assert!(rows.iter().all(|row| row.width() <= 100));
        assert_eq!(
            texts[..6],
            [
                format!("  {HEADER}{}Command", " ".repeat(96 - 30 - 7)),
                "  Would you like to run the following command?".to_owned(),
                String::new(),
                "  $ echo building-the-project-please-wait building-the-project-please-wait"
                    .to_owned(),
                "    building-the-project-please-wait && touch ../PWNED_BY_HIDDEN_TAIL".to_owned(),
                "  ❯ 1. Yes".to_owned(),
            ]
        );
        assert_eq!(
            texts[6],
            "    2. Yes, and don't ask again for this exact command"
        );
        assert_eq!(rows[3].segments()[1].paint, theme().tag);
    }

    #[test]
    fn inline_command_panel_preserves_a_command_wider_than_any_row() {
        let command = format!("printf 'START{}END'", "x".repeat(5000));
        let rows = approval_panel_rows(
            &theme(),
            &command_content(&command),
            &choices(None),
            0,
            80,
            24,
        )
        .rows;
        let joined = texts(&rows).concat();
        assert_eq!(joined.matches('x').count(), 5000, "{joined}");
        assert!(
            joined.contains("'START") && joined.contains("END'"),
            "{joined}"
        );
        assert!(rows.iter().all(|row| row.width() <= 80));
    }
}
