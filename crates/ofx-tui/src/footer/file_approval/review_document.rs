use memchr::memchr;
use ofx_contract::FileChangeStats;
use ofx_markdown::{FileReview, ReviewOp};

const CHECKPOINT_ROWS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewDocument {
    text: Vec<u8>,
    checkpoints: Vec<usize>,
    runs: Vec<Run>,
    rows: usize,
    stats: FileChangeStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    first_row: usize,
    op: ReviewOp,
    label: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DocumentLine<'a> {
    pub(crate) op: ReviewOp,
    pub(crate) label: usize,
    pub(crate) text: &'a [u8],
}

impl DocumentLine<'_> {
    pub(crate) fn number(&self) -> Option<usize> {
        numbered(self.op).then_some(self.label)
    }
}

impl ReviewDocument {
    pub(crate) fn review(before: &[u8], after: &[u8]) -> Self {
        let review = FileReview::new(before, after);
        let mut document = Self::empty(FileChangeStats::from_lines(
            review.additions(),
            review.deletions(),
        ));
        for line in review.rows() {
            let label = match line.op {
                ReviewOp::Elision => line.elision_count,
                ReviewOp::Notice => 0,
                ReviewOp::Context | ReviewOp::Addition | ReviewOp::Deletion => line
                    .new_line
                    .or(line.old_line)
                    .and_then(|number| usize::try_from(number).ok())
                    .unwrap_or_default(),
            };
            document.push(line.op, label, line.text);
        }
        document
    }

    pub(crate) fn notice(text: &str) -> Self {
        let mut document = Self::empty(FileChangeStats::from_lines(0, 0));
        document.push(ReviewOp::Notice, 0, text.as_bytes());
        document
    }

    fn empty(stats: FileChangeStats) -> Self {
        Self {
            text: Vec::new(),
            checkpoints: Vec::new(),
            runs: Vec::new(),
            rows: 0,
            stats,
        }
    }

    fn push(&mut self, op: ReviewOp, label: usize, text: &[u8]) {
        if self.rows.is_multiple_of(CHECKPOINT_ROWS) {
            self.checkpoints.push(self.text.len());
        }
        let continues = self.runs.last().is_some_and(|run| {
            run.op == op && numbered(op) && run.label + (self.rows - run.first_row) == label
        });
        if !continues {
            self.runs.push(Run {
                first_row: self.rows,
                op,
                label,
            });
        }
        self.text.extend_from_slice(text);
        self.text.push(b'\n');
        self.rows += 1;
    }

    pub(crate) fn len(&self) -> usize {
        self.rows
    }

    pub(crate) fn stats(&self) -> FileChangeStats {
        self.stats
    }

    pub(crate) fn lines_from(&self, start: usize) -> Lines<'_> {
        let row = start.min(self.rows);
        let checkpoint = row / CHECKPOINT_ROWS;
        let mut lines = Lines {
            document: self,
            row: checkpoint * CHECKPOINT_ROWS,
            offset: self
                .checkpoints
                .get(checkpoint)
                .copied()
                .unwrap_or_default(),
            run: 0,
        };
        while lines.row < row {
            lines.skip_line();
        }
        lines.run = self
            .runs
            .partition_point(|run| run.first_row <= row)
            .saturating_sub(1);
        lines
    }
}

fn numbered(op: ReviewOp) -> bool {
    matches!(
        op,
        ReviewOp::Context | ReviewOp::Addition | ReviewOp::Deletion
    )
}

pub(crate) struct Lines<'a> {
    document: &'a ReviewDocument,
    row: usize,
    offset: usize,
    run: usize,
}

impl Lines<'_> {
    fn skip_line(&mut self) -> usize {
        let text = &self.document.text[self.offset..];
        let end = memchr(b'\n', text).unwrap_or(text.len());
        self.offset += end + 1;
        self.row += 1;
        end
    }
}

impl<'a> Iterator for Lines<'a> {
    type Item = DocumentLine<'a>;

    fn next(&mut self) -> Option<DocumentLine<'a>> {
        if self.row >= self.document.rows {
            return None;
        }
        let runs = &self.document.runs;
        while runs
            .get(self.run + 1)
            .is_some_and(|run| run.first_row <= self.row)
        {
            self.run += 1;
        }
        let run = runs[self.run];
        let row = self.row;
        let start = self.offset;
        let end = start + self.skip_line();
        let label = if numbered(run.op) {
            run.label + (row - run.first_row)
        } else {
            run.label
        };
        Some(DocumentLine {
            op: run.op,
            label,
            text: &self.document.text[start..end],
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn numbered_lines(prefix: &str, lines: std::ops::Range<usize>) -> String {
        lines.fold(String::new(), |mut text, line| {
            let _ = writeln!(text, "{prefix}{line}");
            text
        })
    }

    fn shown(lines: Lines<'_>) -> Vec<(ReviewOp, usize, String)> {
        lines
            .map(|line| {
                (
                    line.op,
                    line.label,
                    String::from_utf8_lossy(line.text).into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn the_document_holds_every_review_row_with_its_number_or_elided_count() {
        let lead = numbered_lines("lead-", 1..9);
        let before = format!("{lead}old\ntail\n");
        let after = format!("{lead}new\nnewer\ntail\n");
        let document = ReviewDocument::review(before.as_bytes(), after.as_bytes());
        assert_eq!(
            document.stats(),
            FileChangeStats {
                additions: 2,
                deletions: 1
            }
        );
        let review = FileReview::new(before.as_bytes(), after.as_bytes());
        let expected: Vec<_> = review
            .rows()
            .map(|line| {
                let label = match line.op {
                    ReviewOp::Elision => line.elision_count,
                    _ => line
                        .new_line
                        .or(line.old_line)
                        .and_then(|number| usize::try_from(number).ok())
                        .unwrap_or_default(),
                };
                (
                    line.op,
                    label,
                    String::from_utf8_lossy(line.text).into_owned(),
                )
            })
            .collect();
        assert_eq!(shown(document.lines_from(0)), expected);
        assert_eq!(
            shown(document.lines_from(0))[..3],
            [
                (ReviewOp::Elision, 3, String::new()),
                (ReviewOp::Context, 4, "lead-4".to_owned()),
                (ReviewOp::Context, 5, "lead-5".to_owned()),
            ]
        );
        assert_eq!(document.len(), expected.len());
    }

    #[test]
    fn lines_can_start_at_any_row_of_a_long_document() {
        let before = numbered_lines("old-", 0..300);
        let after = numbered_lines("new-", 0..300).replace("new-150\n", "");
        let document = ReviewDocument::review(before.as_bytes(), after.as_bytes());
        let whole = shown(document.lines_from(0));
        assert!(document.len() > 4 * CHECKPOINT_ROWS);
        for start in 0..=document.len() {
            assert_eq!(shown(document.lines_from(start)), whole[start..], "{start}");
        }
        assert_eq!(shown(document.lines_from(usize::MAX)), []);
    }

    #[test]
    fn an_unchanged_file_and_an_unread_file_show_one_notice() {
        let same = ReviewDocument::review(b"same\n", b"same\n");
        assert_eq!(
            shown(same.lines_from(0)),
            [(ReviewOp::Notice, 0, "No content changes".to_owned())]
        );
        let unread = ReviewDocument::notice("cannot be shown");
        assert_eq!(
            shown(unread.lines_from(0)),
            [(ReviewOp::Notice, 0, "cannot be shown".to_owned())]
        );
        assert_eq!(unread.stats(), FileChangeStats::from_lines(0, 0));
    }

    #[test]
    fn a_review_of_many_lines_keeps_one_run_per_contiguous_change() {
        let after = numbered_lines("line-", 0..50_000);
        let document = ReviewDocument::review(b"", after.as_bytes());
        assert_eq!(document.len(), 50_000);
        assert_eq!(document.runs.len(), 1);
        assert_eq!(
            document.checkpoints.len(),
            50_000_usize.div_ceil(CHECKPOINT_ROWS)
        );
        let line = document.lines_from(49_999).next().unwrap();
        assert_eq!(
            (line.op, line.number(), line.text),
            (ReviewOp::Addition, Some(50_000), &b"line-49999"[..])
        );
    }
}
