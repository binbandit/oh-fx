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

#[test]
fn line_counts_match_upstream_file_change_stats() {
    let counts = |old: &str, new: &str| {
        let review = FileReview::new(old.as_bytes(), new.as_bytes());
        (review.additions(), review.deletions())
    };
    assert_eq!(counts("a\nb\nc\n", "a\nb\nc\n"), (0, 0));
    assert_eq!(counts("", ""), (0, 0));
    assert_eq!(counts("", "a\nb\n"), (2, 0));
    assert_eq!(counts("a\nb\n", ""), (0, 2));
    assert_eq!(counts("alpha\nbeta\n", "alpha\ngamma\ndelta\n"), (2, 1));
    assert_eq!(counts("a\nb\nc\nd\n", "a\nx\nc\ny\n"), (2, 2));
    assert_eq!(counts("a\nb\nc\n", "c\nb\na\n"), (2, 2));
    assert_eq!(counts("one", "one\ntwo"), (1, 0));
    assert_eq!(counts("a\nb", "a\nb\n"), (1, 0));
    assert_eq!(counts("a\nb\n", "a\nb"), (0, 1));
    assert_eq!(counts("", "a"), (1, 0));
    assert_eq!(
        counts(
            "x\n(trailing newline added)",
            "x\n(trailing newline added)\n"
        ),
        (1, 0)
    );
    let old = format!(
        "{}old\n{}",
        numbered("line ", 0..5000),
        numbered("line ", 5000..10_000)
    );
    let new = format!(
        "{}new one\nnew two\n{}",
        numbered("line ", 0..5000),
        numbered("line ", 5000..10_000)
    );
    assert_eq!(counts(&old, &new), (2, 1));
    let forward = numbered("", 0..9000);
    let mut reversed = String::new();
    for line in forward.lines().rev() {
        let _ = writeln!(reversed, "{line}");
    }
    assert_eq!(counts(&forward, &reversed), (9000, 9000));
    assert_eq!(counts(&forward, forward.trim_end_matches('\n')), (0, 1));
}

fn upstream_compute(old_text: &[u8], new_text: &[u8]) -> Vec<(LineOp, Vec<u8>)> {
    let old_marker = trailing_newline_marker(old_text, new_text, true);
    let new_marker = trailing_newline_marker(old_text, new_text, false);
    let old_lines: Vec<&[u8]> = text_lines(old_text).chain(old_marker).collect();
    let new_lines: Vec<&[u8]> = text_lines(new_text).chain(new_marker).collect();
    let stride = new_lines.len() + 1;
    let mut table = vec![0_u32; (old_lines.len() + 1) * stride];
    for old_index in 1..=old_lines.len() {
        for new_index in 1..=new_lines.len() {
            table[old_index * stride + new_index] =
                if old_lines[old_index - 1] == new_lines[new_index - 1] {
                    table[(old_index - 1) * stride + new_index - 1] + 1
                } else {
                    table[old_index * stride + new_index - 1]
                        .max(table[(old_index - 1) * stride + new_index])
                };
        }
    }
    let mut result = Vec::new();
    let mut old_cursor = old_lines.len();
    let mut new_cursor = new_lines.len();
    while old_cursor > 0 || new_cursor > 0 {
        if old_cursor > 0
            && new_cursor > 0
            && old_lines[old_cursor - 1] == new_lines[new_cursor - 1]
        {
            result.push((LineOp::Equal, old_lines[old_cursor - 1].to_vec()));
            old_cursor -= 1;
            new_cursor -= 1;
        } else if new_cursor > 0
            && (old_cursor == 0
                || table[old_cursor * stride + new_cursor - 1]
                    >= table[(old_cursor - 1) * stride + new_cursor])
        {
            result.push((LineOp::Add, new_lines[new_cursor - 1].to_vec()));
            new_cursor -= 1;
        } else {
            result.push((LineOp::Remove, old_lines[old_cursor - 1].to_vec()));
            old_cursor -= 1;
        }
    }
    result.reverse();
    result
}

struct Lines(u64);

impl Lines {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }

    fn text(&mut self, alphabet: u64) -> Vec<u8> {
        let count = self.next(24);
        let mut text = Vec::new();
        for _ in 0..count {
            match self.next(alphabet + 1) {
                0 => {}
                letter => text.push(b'a' + u8::try_from(letter).unwrap()),
            }
            text.push(b'\n');
        }
        if self.next(4) == 0 {
            text.pop();
        }
        text
    }
}

#[test]
fn compute_keeps_upstream_operations_for_any_line_sequences() {
    let mut lines = Lines(0x5eed);
    for round in 0..4_000 {
        let alphabet = [1, 2, 3, 5, 9][round % 5];
        let old = lines.text(alphabet);
        let new = match round % 4 {
            0 => old.clone(),
            1 => {
                let mut edited = old.clone();
                let tail = lines.text(alphabet);
                edited.extend_from_slice(&tail);
                edited
            }
            _ => lines.text(alphabet),
        };
        let fast: Vec<(LineOp, Vec<u8>)> = compute(
            &old,
            &new,
            trailing_newline_marker(&old, &new, true),
            trailing_newline_marker(&old, &new, false),
        )
        .iter()
        .map(|line| (line.op, line.text.to_vec()))
        .collect();
        assert_eq!(
            fast,
            upstream_compute(&old, &new),
            "{:?} -> {:?}",
            String::from_utf8_lossy(&old),
            String::from_utf8_lossy(&new)
        );
    }
}

#[test]
fn compute_keeps_upstream_operations_around_shared_prefixes_and_suffixes() {
    for (old, new) in [
        ("a\n", "a\na\n"),
        ("a\na\n", "a\n"),
        ("a\nb\na\n", "a\na\nb\na\n"),
        ("x\na\nb\n", "x\nb\na\nb\n"),
        ("a\nb\nc\nd\n", "a\nc\nb\nd\n"),
        ("p\nq\np\nq\n", "p\nq\n"),
        ("same\nsame\nsame\n", "same\nother\nsame\n"),
        ("a\nb", "a\nb\n"),
        ("a\nb\n", "a\nc"),
    ] {
        let fast: Vec<(LineOp, Vec<u8>)> = computed(old, new)
            .into_iter()
            .map(|(op, text)| (op, text.into_bytes()))
            .collect();
        assert_eq!(
            fast,
            upstream_compute(old.as_bytes(), new.as_bytes()),
            "{old:?} -> {new:?}"
        );
    }
}

fn numbered_rows(review: &FileReview<'_>) -> Vec<(ReviewOp, Option<u32>, Option<u32>, usize)> {
    review
        .rows()
        .map(|line| (line.op, line.old_line, line.new_line, line.elision_count))
        .collect()
}

#[test]
fn computed_rows_carry_upstreams_line_numbers_and_elision_counts() {
    use ReviewOp::{Addition, Context, Deletion, Elision};
    let lead = numbered("lead-", 1..9);
    let tail = numbered("tail-", 1..9);
    let before = format!("{lead}old\n{tail}");
    let after = format!("{lead}new one\nnew two\n{tail}");
    let review = FileReview::new(before.as_bytes(), after.as_bytes());
    assert!(matches!(review.mode, Mode::Computed(_)));
    let rows = numbered_rows(&review);
    assert_eq!(rows[0], (Elision, None, None, 3));
    assert_eq!(rows[1], (Context, Some(4), Some(4), 0));
    assert_eq!(rows[5], (Context, Some(8), Some(8), 0));
    assert_eq!(rows[6], (Deletion, Some(9), None, 0));
    assert_eq!(rows[7], (Addition, None, Some(9), 0));
    assert_eq!(rows[8], (Addition, None, Some(10), 0));
    assert_eq!(rows[9], (Context, Some(10), Some(11), 0));
    assert_eq!(rows[13], (Context, Some(14), Some(15), 0));
    assert_eq!(rows[14], (Elision, None, None, 3));
    assert_eq!(rows.len(), 15);
    let marker = FileReview::new(b"a\nb\n", b"a\nb");
    assert_eq!(
        numbered_rows(&marker),
        [
            (Context, Some(1), Some(1), 0),
            (Context, Some(2), Some(2), 0),
            (Deletion, Some(3), None, 0)
        ]
    );
}

#[test]
fn fallback_rows_carry_upstreams_line_numbers_and_elision_counts() {
    use ReviewOp::{Addition, Context, Deletion, Elision};
    let before = format!(
        "{}old\n{}",
        numbered("line-", 1..1001),
        numbered("tail-", 1..1001)
    );
    let after = format!(
        "{}new\nnewer\n{}",
        numbered("line-", 1..1001),
        numbered("tail-", 1..1001)
    );
    let review = FileReview::new(before.as_bytes(), after.as_bytes());
    assert!(matches!(review.mode, Mode::Fallback(_)));
    let rows = numbered_rows(&review);
    assert_eq!(
        rows,
        [
            (Elision, None, None, 995),
            (Context, Some(996), Some(996), 0),
            (Context, Some(997), Some(997), 0),
            (Context, Some(998), Some(998), 0),
            (Context, Some(999), Some(999), 0),
            (Context, Some(1000), Some(1000), 0),
            (Deletion, Some(1001), None, 0),
            (Addition, None, Some(1001), 0),
            (Addition, None, Some(1002), 0),
            (Context, Some(1002), Some(1003), 0),
            (Context, Some(1003), Some(1004), 0),
            (Context, Some(1004), Some(1005), 0),
            (Context, Some(1005), Some(1006), 0),
            (Context, Some(1006), Some(1007), 0),
            (Elision, None, None, 995),
        ]
    );
    let old = numbered("old-", 1..20_000);
    let new = numbered("new-", 1..20_000);
    let markers = FileReview::new(old.as_bytes(), new.trim_end().as_bytes());
    let rows: Vec<_> = markers.rows().collect();
    let added = rows[rows.len() - 2];
    let removed = rows[rows.len() - 1];
    assert_eq!(
        (removed.op, removed.old_line, removed.text),
        (Deletion, Some(20_000), &b"(trailing newline removed)"[..])
    );
    assert_eq!(
        (added.op, added.new_line, added.text),
        (Addition, Some(19_999), &b"new-19999"[..])
    );
}
