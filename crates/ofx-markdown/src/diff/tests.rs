use std::fmt::Write;

use super::*;

fn computed(old: &str, new: &str) -> Vec<(LineOp, String)> {
    let old = old.as_bytes();
    let new = new.as_bytes();
    compute(
        old,
        new,
        trailing_newline_marker(old, new, true),
        trailing_newline_marker(old, new, false),
    )
    .iter()
    .map(|line| (line.op, String::from_utf8_lossy(line.text).into_owned()))
    .collect()
}

fn expected(lines: &[(LineOp, &str)]) -> Vec<(LineOp, String)> {
    lines
        .iter()
        .map(|(op, text)| (*op, (*text).to_owned()))
        .collect()
}

fn rows(review: &FileReview<'_>) -> Vec<(ReviewOp, String)> {
    review
        .rows()
        .map(|line| (line.op, String::from_utf8_lossy(line.text).into_owned()))
        .collect()
}

fn numbered(prefix: &str, range: std::ops::Range<usize>) -> String {
    let mut text = String::new();
    for number in range {
        let _ = writeln!(text, "{prefix}{number:04}");
    }
    text
}

#[test]
fn compute_matches_upstream_line_operations() {
    use LineOp::{Add, Equal, Remove};
    assert_eq!(
        computed("a\nb\nc\n", "a\nb\nc\n"),
        expected(&[(Equal, "a"), (Equal, "b"), (Equal, "c")])
    );
    assert_eq!(
        computed("a\nb\n", "a\nX\nb\n"),
        expected(&[(Equal, "a"), (Add, "X"), (Equal, "b")])
    );
    assert_eq!(
        computed("a\nb\nc\n", "a\nc\n"),
        expected(&[(Equal, "a"), (Remove, "b"), (Equal, "c")])
    );
    assert_eq!(
        computed("a\nold\nc\n", "a\nnew\nc\n"),
        expected(&[(Equal, "a"), (Remove, "old"), (Add, "new"), (Equal, "c")])
    );
    assert_eq!(
        computed("", "first\nsecond\n"),
        expected(&[(Add, "first"), (Add, "second")])
    );
    assert_eq!(
        computed("a\n", "a"),
        expected(&[(Equal, "a"), (Remove, "(trailing newline removed)")])
    );
    assert_eq!(
        computed("a", "a\n"),
        expected(&[(Equal, "a"), (Add, "(trailing newline added)")])
    );
}

#[test]
fn file_review_exposes_every_changed_line_beyond_the_bounded_preview_cap() {
    let after = numbered("line ", 1..31);
    let review = FileReview::new(b"", after.as_bytes());
    let rows = rows(&review);
    assert_eq!(rows.len(), 30);
    assert_eq!(rows[24], (ReviewOp::Addition, "line 0025".to_owned()));
    assert_eq!(rows[29], (ReviewOp::Addition, "line 0030".to_owned()));
}

#[test]
fn fallback_file_review_seeks_to_replacement_boundaries() {
    let before = numbered("old-", 1..2049);
    let after = numbered("new-", 1..2049);
    let review = FileReview::new(before.as_bytes(), after.as_bytes());
    assert!(matches!(review.mode, Mode::Fallback(_)));
    let rows = rows(&review);
    assert_eq!(rows.len(), 4096);
    assert_eq!(rows[2047], (ReviewOp::Deletion, "old-2048".to_owned()));
    assert_eq!(rows[2048], (ReviewOp::Addition, "new-0001".to_owned()));
    assert_eq!(rows[4094].1, "new-2047");
    assert_eq!(rows[4095].1, "new-2048");
}

#[test]
fn file_review_projects_five_context_lines_and_exact_computed_elisions() {
    let lead = numbered("lead-", 1..9);
    let middle = numbered("middle-", 1..13);
    let tail = numbered("tail-", 1..9);
    let before = format!("{lead}old-one\n{middle}old-two\n{tail}");
    let after = format!("{lead}new-one\n{middle}new-two\n{tail}");
    let review = FileReview::new(before.as_bytes(), after.as_bytes());
    let rows = rows(&review);
    assert_eq!(rows.len(), 27);
    assert_eq!(rows[0], (ReviewOp::Elision, String::new()));
    for (index, row) in rows[1..6].iter().enumerate() {
        assert_eq!(*row, (ReviewOp::Context, format!("lead-{:04}", index + 4)));
    }
    assert_eq!(rows[6], (ReviewOp::Deletion, "old-one".to_owned()));
    assert_eq!(rows[7], (ReviewOp::Addition, "new-one".to_owned()));
    assert_eq!(rows[8].1, "middle-0001");
    assert_eq!(rows[12].1, "middle-0005");
    assert_eq!(rows[13].0, ReviewOp::Elision);
    assert_eq!(rows[14].1, "middle-0008");
    assert_eq!(rows[18].1, "middle-0012");
    assert_eq!(rows[19], (ReviewOp::Deletion, "old-two".to_owned()));
    assert_eq!(rows[20], (ReviewOp::Addition, "new-two".to_owned()));
    assert_eq!(rows[21].1, "tail-0001");
    assert_eq!(rows[25].1, "tail-0005");
    assert_eq!(rows[26].0, ReviewOp::Elision);
}

#[test]
fn file_review_projects_only_proven_fallback_context_with_exact_elisions() {
    let before = numbered("line-", 1..1001);
    let after = format!(
        "{}inserted-context-a\ninserted-context-b\n{}",
        numbered("line-", 1..501),
        numbered("line-", 501..1001)
    );
    let review = FileReview::new(before.as_bytes(), after.as_bytes());
    assert!(matches!(review.mode, Mode::Fallback(_)));
    let rows = rows(&review);
    assert_eq!(rows.len(), 14);
    assert_eq!(rows[0].0, ReviewOp::Elision);
    assert_eq!(rows[1], (ReviewOp::Context, "line-0496".to_owned()));
    assert_eq!(rows[5], (ReviewOp::Context, "line-0500".to_owned()));
    assert_eq!(
        rows[6],
        (ReviewOp::Addition, "inserted-context-a".to_owned())
    );
    assert_eq!(
        rows[7],
        (ReviewOp::Addition, "inserted-context-b".to_owned())
    );
    assert_eq!(rows[8], (ReviewOp::Context, "line-0501".to_owned()));
    assert_eq!(rows[12], (ReviewOp::Context, "line-0505".to_owned()));
    assert_eq!(rows[13].0, ReviewOp::Elision);
    assert_eq!((review.additions(), review.deletions()), (2, 0));
}

#[test]
fn file_review_preserves_replacements_and_trailing_newline_markers() {
    let replacement = FileReview::new(b"old\n", b"new\n");
    assert_eq!(
        rows(&replacement),
        [
            (ReviewOp::Deletion, "old".to_owned()),
            (ReviewOp::Addition, "new".to_owned())
        ]
    );
    let newline_change = FileReview::new(b"a\n", b"a");
    assert_eq!(
        rows(&newline_change),
        [
            (ReviewOp::Context, "a".to_owned()),
            (ReviewOp::Deletion, "(trailing newline removed)".to_owned())
        ]
    );
}

#[test]
fn unchanged_content_reviews_as_a_single_notice() {
    let review = FileReview::new(b"same\n", b"same\n");
    assert_eq!(
        rows(&review),
        [(ReviewOp::Notice, "No content changes".to_owned())]
    );
}
