use std::fmt::Write as _;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;

use ofx_contract::{
    ApprovalScope, CallDescription, Concurrency, PathAccess, RequestId, SessionGrant, ToolActivity,
    ToolCallId, ToolEffect,
};
use ofx_text::encode_terminal_safe_path_tail;

use super::*;

fn request(
    tool: &str,
    (file, change): (FileMutation, Option<ProposedFileChange>),
    always: Option<SessionGrant>,
) -> ApprovalRequest {
    ApprovalRequest {
        id: RequestId::new(1),
        tool_name: tool.to_owned(),
        call_id: ToolCallId::new("call-1"),
        description: CallDescription {
            title: "Editing note.txt".to_owned(),
            label: None,
            activity: ToolActivity::Edit,
            effect: ToolEffect::Irreversible,
            concurrency: Concurrency::Serial,
        },
        tool_arguments_preview: String::new(),
        tool_arguments_truncated: false,
        scope: ApprovalScope {
            target: None,
            access: PathAccess::WorkspaceOnly,
            always,
        },
        command: None,
        file: Some(file),
        change,
    }
}

fn proposed(path: &[u8], before: Option<&[u8]>, after: &[u8]) -> ProposedFileChange {
    ProposedFileChange {
        display_path: encode_terminal_safe_path_tail(path, 4096).unwrap(),
        before: before.map(Arc::from),
        after: Arc::from(after),
    }
}

fn change(path: &str, before: &str, after: &str) -> (FileMutation, Option<ProposedFileChange>) {
    let mutation = FileMutation {
        target: PathBuf::from("/ws").join(path),
        state: if before == after {
            FileMutationState::Unchanged
        } else if before.is_empty() {
            FileMutationState::Creates
        } else {
            FileMutationState::Changes
        },
    };
    let before = (!before.is_empty()).then_some(before.as_bytes());
    let change = proposed(path.as_bytes(), before, after.as_bytes());
    (mutation, Some(change))
}

fn file_approval(request: &ApprovalRequest) -> FileApproval {
    FileApproval::new(
        request,
        request.file.as_ref().unwrap(),
        request.change.as_ref(),
    )
}

fn approval(tool: &str, path: &str, before: &str, after: &str) -> FileApproval {
    file_approval(&request(
        tool,
        change(path, before, after),
        Some(SessionGrant::WorkspaceFiles),
    ))
}

fn frame(cols: usize, screen_rows: usize) -> PanelFrame<'static> {
    PanelFrame {
        cols,
        terminal_rows: u16::try_from(screen_rows + 2).unwrap(),
        inline_rows: screen_rows,
        screen_rows,
        scroll: usize::MAX,
        seen: &[],
    }
}

fn theme() -> Theme {
    Theme::builtin(false, false, true)
}

fn view(file: &FileApproval, frame: PanelFrame<'_>) -> PanelView {
    view_after(file, frame, false)
}

fn view_after(file: &FileApproval, frame: PanelFrame<'_>, change_seen: bool) -> PanelView {
    let layout = ReviewLayout::measure(file, frame.cols);
    file_approval_rows(
        &theme(),
        file,
        &layout,
        &file.choices(),
        0,
        frame,
        change_seen,
    )
}

fn texts(rows: &[Row]) -> Vec<String> {
    rows.iter().map(Row::text).collect()
}

fn numbered(prefix: &str, lines: RangeInclusive<usize>) -> String {
    lines.fold(String::new(), |mut text, line| {
        let _ = writeln!(text, "{prefix}-{line:02}");
        text
    })
}

const HEADER_WIDE: &str = "  Permission needed · Review change";

#[test]
fn a_fitting_review_follows_upstreams_document_rows() {
    let file = approval(
        "edit_file",
        "docs/notes.md",
        "alpha\nbeta\ngamma\n",
        "alpha\nBETA\ngamma\n",
    );
    let shown = view(&file, frame(80, 22));
    let divider = "─".repeat(80);
    let header = format!("{HEADER_WIDE}{}Edit · +1  -1", " ".repeat(78 - 35 - 13));
    assert_eq!(
        texts(&shown.rows),
        [
            "",
            divider.as_str(),
            "      1   alpha",
            "      2 - beta",
            "      2 + BETA",
            "      3   gamma",
            "┄".repeat(80).as_str(),
            header.as_str(),
            "",
            "  docs/notes.md  ·  Apply this change?",
            "",
            "  ❯ 1  Apply once",
            "    2  Apply + allow workspace file access for this session",
            "    3  Don't apply",
            "",
            divider.as_str(),
            "  1–3 choose now    ↑↓ or tab options    enter confirm    esc cancel",
        ]
    );
    assert_eq!(
        shown.review,
        Review {
            required_rows: 2..14,
            window: 0..4,
            action_rows: 4,
            complete: true,
            screen: false,
            change_shown: Some(true),
        }
    );
}

#[test]
fn review_rows_paint_the_text_and_accent_only_the_number_and_sign() {
    let file = approval("edit_file", "note.txt", "old\n", "new\n");
    let neutral = theme();
    let rows = view(&file, frame(80, 22)).rows;
    let removed = rows[2].segments();
    assert_eq!(rows[2].text(), "      1 - old");
    assert_eq!(removed[1].text, "1");
    assert_eq!(removed[1].paint, neutral.statusline);
    assert_eq!(removed[3].text, "-");
    assert_eq!(removed[3].paint, neutral.red);
    assert_eq!(removed[5].text, "old");
    assert_eq!(removed[5].paint, neutral.red);
    let pinned = Theme::builtin(false, true, true);
    let layout = ReviewLayout::measure(&file, 80);
    let rows = file_approval_rows(
        &pinned,
        &file,
        &layout,
        &file.choices(),
        0,
        frame(80, 22),
        false,
    )
    .rows;
    let (added_marker, _) = pinned.diff_marker_paints().unwrap();
    let added = rows[3].segments();
    assert_eq!(rows[3].text(), "      1 + new");
    assert_eq!(added[1].paint, added_marker);
    assert_eq!(added[3].paint, added_marker);
    assert_eq!(added[5].paint, pinned.green);
}

#[test]
fn unchanged_runs_show_exact_elision_markers_without_a_number() {
    let lead = numbered("lead", 1..=8);
    let tail = numbered("tail", 1..=8);
    let file = approval(
        "edit_file",
        "note.txt",
        &format!("{lead}old\n{tail}"),
        &format!("{lead}new\n{tail}"),
    );
    let shown = texts(&view(&file, frame(80, 40)).rows);
    assert_eq!(shown[2], "        ⋯ 3 unchanged lines ⋯");
    assert_eq!(shown[3], "      4   lead-04");
    assert_eq!(shown[15], "        ⋯ 3 unchanged lines ⋯");
    assert!(shown.iter().all(|row| !row.contains("0 ⋯")));
}

#[test]
fn notices_and_wrapped_lines_keep_upstreams_gutters() {
    let same = approval("write_file", "note.txt", "same\n", "same\n");
    let shown = texts(&view(&same, frame(80, 22)).rows);
    assert_eq!(shown[2], "         No content changes");
    assert!(shown[4].ends_with("Check"), "{}", shown[4]);
    assert_eq!(
        shown[6],
        "  note.txt  ·  Reveal that this file already matches?"
    );
    assert_eq!(shown[8], "  ❯ 1  Reveal once");
    assert_eq!(
        shown[9],
        "    2  Reveal + allow workspace file access for this session"
    );
    assert_eq!(shown[10], "    3  Don't reveal");
    let wrapped = approval("write_file", "n", "", "abcdefgh\n");
    let layout = ReviewLayout::measure(&wrapped, 12);
    let (rows, change_shown) = review_window(&theme(), &wrapped, &layout, &(0..layout.rows()));
    assert!(change_shown);
    assert_eq!(
        texts(&rows),
        [
            "      1 + ab",
            "          cd",
            "          ef",
            "          gh"
        ]
    );
}

#[test]
fn file_content_is_escaped_before_it_reaches_a_row() {
    let file = approval(
        "write_file",
        "note.txt",
        "",
        "\u{1b}[31mred\u{1b}]0;title\u{7}\tx\u{200b}\n",
    );
    let shown = texts(&view(&file, frame(80, 22)).rows);
    assert_eq!(
        shown[2],
        "      1 + \\x1b[31mred\\x1b]0;title\\x07\\x09x\\u{200b}"
    );
    let mut raw = b"bad \xff byte\n".to_vec();
    raw.extend_from_slice("\u{202e}txt.exe\n".as_bytes());
    let request = request(
        "write_file",
        (
            FileMutation {
                target: PathBuf::from("/ws/a\u{1b}b"),
                state: FileMutationState::Creates,
            },
            Some(proposed(b"a\x1bb", None, &raw)),
        ),
        None,
    );
    let file = file_approval(&request);
    let shown = texts(&view(&file, frame(80, 22)).rows);
    assert_eq!(shown[2], "      1 + bad \\xff byte");
    assert_eq!(shown[3], "      2 + \\u{202e}txt.exe");
    assert_eq!(shown[7], "  a\\x1bb  ·  Apply this change?");
    assert!(shown.iter().all(|row| !row.contains('\u{1b}')));
}

#[test]
fn a_tall_review_scrolls_in_a_window_from_its_tail_above_fixed_controls() {
    let file = approval("write_file", "new.txt", "", &numbered("line", 1..=40));
    let shown = view(&file, frame(80, 22));
    let rows = texts(&shown.rows);
    assert_eq!(rows.len(), 22);
    assert_eq!(rows[0], "     30 + line-30");
    assert_eq!(rows[10], "     40 + line-40");
    assert_eq!(rows[16], "  ❯ 1  Apply once");
    assert_eq!(
        rows[17],
        "    2  Apply + allow workspace file access for this session"
    );
    assert_eq!(rows[18], "    3  Don't apply");
    assert!(rows[21].contains("pgup/pgdn scroll"), "{}", rows[21]);
    assert_eq!(shown.review.window, 29..40);
    assert_eq!(shown.review.required_rows, 0..19);
    assert!(shown.review.screen && shown.review.complete);
    assert_eq!(shown.review.change_shown, Some(true));
    let top = view(
        &file,
        PanelFrame {
            scroll: 0,
            ..frame(80, 22)
        },
    );
    let rows = texts(&top.rows);
    assert_eq!(rows[0], "      1 + line-01");
    assert_eq!(rows[16], "  ❯ 1  Apply once");
    assert_eq!(top.review.window, 0..11);
}

#[test]
fn a_yes_waits_for_a_changed_line_and_not_for_every_row() {
    let file = approval(
        "edit_file",
        "note.txt",
        &numbered("line", 1..=40),
        &format!("line-00\n{}", numbered("line", 2..=40)),
    );
    let tail = view(&file, frame(80, 10));
    let rows = texts(&tail.rows);
    assert_eq!(rows[0], "      6   line-06");
    assert_eq!(rows[1], "        ⋯ 34 unchanged lines ⋯");
    assert_eq!(rows[5], "  ❯ ! 1  Apply once · scroll to review");
    assert_eq!(tail.review.change_shown, Some(false));
    assert!(tail.review.complete);
    let change = view(
        &file,
        PanelFrame {
            scroll: 0,
            ..frame(80, 10)
        },
    );
    let rows = texts(&change.rows);
    assert_eq!(rows[0], "      1 - line-01");
    assert_eq!(rows[1], "      1 + line-00");
    assert_eq!(rows[5], "  ❯ 1  Apply once");
    assert_eq!(change.review.change_shown, Some(true));
    let back = texts(&view_after(&file, frame(80, 10), true).rows);
    assert_eq!(back[1], "        ⋯ 34 unchanged lines ⋯");
    assert_eq!(back[5], "  ❯ 1  Apply once");
}

#[test]
fn the_controls_compact_on_shorter_terminals_and_leave_only_choices_on_tiny_ones() {
    let file = approval("write_file", "new.txt", "", &numbered("line", 1..=40));
    let compact = texts(&view(&file, frame(80, 10)).rows);
    assert_eq!(compact.len(), 10);
    assert_eq!(compact[0], "     39 + line-39");
    assert!(compact[2].starts_with('┄'));
    assert!(compact[3].starts_with(HEADER_WIDE));
    assert_eq!(compact[4], "  new.txt  ·  Apply this change?");
    assert_eq!(compact[5], "  ❯ 1  Apply once");
    assert!(compact[8].starts_with('─'));
    let tiny = view(&file, frame(80, 7));
    assert_eq!(
        texts(&tiny.rows),
        [
            "  ❯ ! 1  Apply once · resize to review",
            "    ! 2  Apply + allow workspace file access for this session",
            "    3  Don't apply",
        ]
    );
    assert!(!tiny.review.complete);
    let none = view(&file, frame(80, 8));
    assert_eq!(texts(&none.rows)[0], "┄".repeat(80));
    assert!(!none.review.complete);
}

#[test]
fn rows_never_overflow_and_narrow_controls_refuse_a_yes() {
    let file = approval("edit_file", "src/profile.txt", "old\n", "new\n");
    for cols in [1, 7, 12, 32, 96] {
        for rows in [3, 8, 12, 22] {
            let shown = view(&file, frame(cols, rows));
            assert!(
                shown.rows.iter().all(|row| row.width() <= cols),
                "{cols} {rows}"
            );
        }
    }
    assert!(view(&file, frame(96, 22)).review.complete);
    assert!(!view(&file, frame(12, 22)).review.complete);
    let narrow = texts(&view(&file, frame(40, 22)).rows);
    assert_eq!(narrow[14], "  enter confirm    esc cancel");
}

#[test]
fn the_header_keeps_the_basename_whole_and_shows_stats_only_beside_it() {
    let file = approval(
        "edit_file",
        "src/ui/footer/approval_ui.zig",
        &"x\n".repeat(345),
        &"y\n".repeat(12),
    );
    let header = project_header(&file, 96);
    assert!(header.complete());
    assert_eq!(
        header.stats,
        Some(FileChangeStats {
            additions: 12,
            deletions: 345
        })
    );
    let shown = texts(&view(&file, frame(96, 40)).rows);
    let header_row = shown
        .iter()
        .find(|row| row.starts_with(HEADER_WIDE))
        .unwrap();
    assert!(header_row.ends_with("Edit · +12  -345"), "{header_row}");
    let squeezed = project_header(&file, 34);
    assert_eq!(squeezed.stats, None);
    assert!(squeezed.path.ellipsis && squeezed.path.complete);
    assert!(!project_header(&file, 20).complete());
    let request = request(
        "write_file",
        change("/home/u/external/private.txt", "x\n", "x\n"),
        Some(SessionGrant::FileChangesUnder(PathBuf::from(
            "/home/u/external",
        ))),
    );
    let external = file_approval(&request);
    let choices = external.choices();
    assert_eq!(
        choices[1].label.fit(200).0,
        "Reveal + allow file changes under /home/u/external for this session"
    );
}

#[test]
fn an_unread_file_names_its_target_and_says_its_change_cannot_be_shown() {
    let request = request(
        "edit_file",
        (
            FileMutation {
                target: PathBuf::from("/etc/hosts"),
                state: FileMutationState::Unread,
            },
            None,
        ),
        None,
    );
    let file = file_approval(&request);
    let shown = texts(&view(&file, frame(140, 22)).rows);
    assert_eq!(shown[2], format!("         {UNREAD_NOTICE}"));
    assert_eq!(
        shown[4],
        format!("{HEADER_WIDE}{}Edit", " ".repeat(138 - 35 - 4))
    );
    assert_eq!(shown[6], "  /etc/hosts  ·  Apply this change?");
    assert_eq!(shown[8], "  ❯ 1  Apply once");
    assert_eq!(shown[9], "    3  Don't apply");
}

#[test]
fn measured_plain_lines_wrap_exactly_as_they_are_drawn() {
    let alphabet = b"ab -~{}0\\x1u";
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..400 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let length = usize::try_from(state % 90).unwrap();
        let text: Vec<u8> = (0..length)
            .map(|index| {
                alphabet[(usize::try_from(state >> 8).unwrap() + index * 7) % alphabet.len()]
            })
            .collect();
        let number = usize::try_from(state % 200_000).unwrap() + 1;
        for op in [ReviewOp::Addition, ReviewOp::Context, ReviewOp::Notice] {
            let line = DocumentLine {
                op,
                label: number,
                text: &text,
            };
            for cols in [0, 5, 9, 10, 11, 12, 17, 40, 80] {
                assert_eq!(line_text(&line), approval_text(&text));
                let mut drawn = 0;
                let drawable = for_each_segment(&line, &line_text(&line), cols, |_, _| drawn += 1);
                let escapes = text.contains(&b'\\');
                assert_eq!(
                    plain_row_count(&line, cols),
                    drawable.then_some(drawn).filter(|_| !escapes),
                    "{op:?} {cols} {:?}",
                    String::from_utf8_lossy(&text)
                );
                assert_eq!(line_rows(&line, cols), (drawn, drawable));
            }
        }
    }
}

#[test]
fn every_window_of_a_long_review_matches_the_whole_review() {
    let after = (1..=200).fold(String::new(), |mut text, line| {
        let width = line * 7 % 53;
        let _ = match line % 4 {
            0 => writeln!(text, "{line} {}", "w".repeat(width)),
            1 => writeln!(text, "{line} caf\u{e9} {}", "\u{4e2d}".repeat(width / 3)),
            2 => writeln!(text, "{line}\t\\x41 {}", "e".repeat(width)),
            _ => writeln!(text),
        };
        text
    });
    let file = approval("write_file", "long.txt", "", &after);
    for cols in [24, 41] {
        let layout = ReviewLayout::measure(&file, cols);
        assert!(layout.rows() > 2 * CHECKPOINT_LINES);
        let (whole, _) = review_window(&theme(), &file, &layout, &(0..layout.rows()));
        let whole = texts(&whole);
        assert_eq!(whole.len(), layout.rows());
        for start in (0..layout.rows()).step_by(5) {
            let end = (start + 13).min(layout.rows());
            let (window, change_shown) = review_window(&theme(), &file, &layout, &(start..end));
            assert_eq!(texts(&window), whole[start..end], "{cols} {start}");
            assert!(change_shown);
        }
    }
}
