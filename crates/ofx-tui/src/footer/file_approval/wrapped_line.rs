use std::collections::VecDeque;
use std::ops::ControlFlow;

use super::super::command_text::{approval_text, approval_text_boundary, encoded_token};

pub(super) const CHUNK_BYTES: usize = 4096;
const LOOKAHEAD_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Resume {
    pub(super) raw: usize,
    pub(super) row: usize,
    pub(super) width: usize,
}

impl Resume {
    pub(super) const START: Self = Self {
        raw: 0,
        row: 0,
        width: 0,
    };

    fn shows_row(self, row: usize) -> bool {
        self.row < row || (self.row == row && self.width == 0)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Source<'a> {
    Raw(&'a [u8]),
    Text(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Widths {
    pub(super) first: usize,
    pub(super) rest: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Walked {
    pub(super) rows: usize,
    pub(super) drawable: bool,
}

pub(super) fn resume_for(resumes: &[Resume], row: usize) -> Resume {
    let shown = resumes.partition_point(|resume| resume.shows_row(row));
    shown
        .checked_sub(1)
        .and_then(|index| resumes.get(index))
        .copied()
        .unwrap_or(Resume::START)
}

pub(super) fn walk(
    source: Source<'_>,
    widths: Widths,
    from: Resume,
    mut resumed: impl FnMut(Resume),
    mut visit: impl FnMut(usize, &str) -> ControlFlow<()>,
) -> Walked {
    let mut text = Encoded::new(source, from.raw);
    let mut row = from.row;
    let mut width = from.width;
    let mut shown = String::new();
    loop {
        text.fill();
        if let Some(raw) = text.boundary() {
            resumed(Resume { raw, row, width });
        }
        let Some((len, cells)) = text.token() else {
            break;
        };
        let available = if row == 0 { widths.first } else { widths.rest };
        if width + cells > available {
            if width == 0 {
                return Walked {
                    rows: row,
                    drawable: false,
                };
            }
            if visit(row, &shown).is_break() {
                return Walked {
                    rows: row + 1,
                    drawable: true,
                };
            }
            shown.clear();
            row += 1;
            width = 0;
            continue;
        }
        shown.push_str(text.take(len));
        width += cells;
    }
    let _ = visit(row, &shown);
    Walked {
        rows: row + 1,
        drawable: true,
    }
}

struct Encoded<'a> {
    source: Source<'a>,
    next: usize,
    buffer: String,
    position: usize,
    starts: VecDeque<(usize, usize)>,
}

impl<'a> Encoded<'a> {
    fn new(source: Source<'a>, raw: usize) -> Self {
        Self {
            source,
            next: raw,
            buffer: String::new(),
            position: 0,
            starts: VecDeque::new(),
        }
    }

    fn exhausted(&self) -> bool {
        match self.source {
            Source::Raw(raw) => self.next >= raw.len(),
            Source::Text(text) => self.next >= text.len(),
        }
    }

    fn fill(&mut self) {
        while self.buffer.len() - self.position < LOOKAHEAD_BYTES && !self.exhausted() {
            if self.position >= CHUNK_BYTES {
                self.buffer.drain(..self.position);
                for start in &mut self.starts {
                    start.0 -= self.position;
                }
                self.position = 0;
            }
            self.starts.push_back((self.buffer.len(), self.next));
            match self.source {
                Source::Raw(raw) => {
                    let end = (self.next + CHUNK_BYTES..raw.len())
                        .find(|index| approval_text_boundary(raw, *index))
                        .unwrap_or(raw.len());
                    self.buffer.push_str(&approval_text(&raw[self.next..end]));
                    self.next = end;
                }
                Source::Text(text) => {
                    self.buffer.push_str(text);
                    self.next = text.len();
                }
            }
        }
    }

    fn boundary(&mut self) -> Option<usize> {
        while self
            .starts
            .front()
            .is_some_and(|(start, _)| *start < self.position)
        {
            self.starts.pop_front();
        }
        let (start, raw) = self.starts.front().copied()?;
        (start == self.position).then(|| {
            self.starts.pop_front();
            raw
        })
    }

    fn token(&self) -> Option<(usize, usize)> {
        (self.position < self.buffer.len()).then(|| encoded_token(&self.buffer[self.position..]))
    }

    fn take(&mut self, len: usize) -> &str {
        let start = self.position;
        self.position += len;
        &self.buffer[start..self.position]
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::command_text::grapheme_fuzz::{Xorshift, random_clusters};
    use super::super::super::command_text::prefix_terminal_safe_by_width;
    use super::*;

    fn rows_by_prefix(text: &str, widths: Widths) -> (Vec<String>, bool) {
        let mut rows = Vec::new();
        let mut offset = 0;
        while rows.is_empty() || offset < text.len() {
            let available = if rows.is_empty() {
                widths.first
            } else {
                widths.rest
            };
            let segment = prefix_terminal_safe_by_width(&text[offset..], available);
            if offset < text.len() && segment.is_empty() {
                return (rows, false);
            }
            rows.push(segment.to_owned());
            offset += segment.len();
        }
        (rows, true)
    }

    fn walked(source: Source<'_>, widths: Widths, from: Resume) -> (Vec<(usize, String)>, Walked) {
        let mut rows = Vec::new();
        let result = walk(
            source,
            widths,
            from,
            |_| {},
            |row, text| {
                rows.push((row, text.to_owned()));
                ControlFlow::Continue(())
            },
        );
        (rows, result)
    }

    fn resumes(source: Source<'_>, widths: Widths) -> Vec<Resume> {
        let mut found = Vec::new();
        walk(
            source,
            widths,
            Resume::START,
            |resume| found.push(resume),
            |_, _| ControlFlow::Continue(()),
        );
        found
    }

    fn long_raw(seed: u64, pieces: usize) -> Vec<u8> {
        let mut raw = Vec::new();
        let mut rng = Xorshift(seed);
        for cluster in random_clusters(seed, pieces) {
            raw.extend_from_slice(cluster.as_bytes());
            if rng.below(3) == 0 {
                raw.extend_from_slice(b"\t\x1b[2J\xff");
            }
        }
        raw.retain(|byte| *byte != b'\n');
        raw
    }

    #[test]
    fn a_walk_wraps_exactly_as_whole_line_prefixes_do() {
        let mut widths = Xorshift(0x51ed_270b);
        for raw in (0..300).map(|seed| long_raw(seed, 40)) {
            let first = widths.below(30);
            let rest = widths.below(30);
            let limits = Widths { first, rest };
            let (expected, drawable) = rows_by_prefix(&approval_text(&raw), limits);
            let (rows, result) = walked(Source::Raw(&raw), limits, Resume::START);
            assert_eq!(result.drawable, drawable, "{raw:?} {first} {rest}");
            assert_eq!(result.rows, expected.len(), "{raw:?} {first} {rest}");
            let texts: Vec<String> = rows.into_iter().map(|(_, text)| text).collect();
            assert_eq!(texts, expected, "{raw:?} {first} {rest}");
        }
    }

    #[test]
    fn every_resume_point_continues_with_the_rows_of_the_whole_walk() {
        let raw = long_raw(7, 3000);
        assert!(raw.len() > 8 * CHUNK_BYTES);
        let widths = Widths {
            first: 70,
            rest: 70,
        };
        let (whole, _) = walked(Source::Raw(&raw), widths, Resume::START);
        let points = resumes(Source::Raw(&raw), widths);
        assert!(
            points.len() > raw.len() / CHUNK_BYTES / 2,
            "{}",
            points.len()
        );
        for point in points {
            let mut shown = Vec::new();
            walk(
                Source::Raw(&raw),
                widths,
                point,
                |_| {},
                |row, text| {
                    if point.shows_row(row) {
                        shown.push((row, text.to_owned()));
                    }
                    if shown.len() < 4 {
                        ControlFlow::Continue(())
                    } else {
                        ControlFlow::Break(())
                    }
                },
            );
            let from = whole
                .iter()
                .position(|(row, _)| point.shows_row(*row))
                .unwrap_or(whole.len());
            assert_eq!(shown, whole[from..from + shown.len()], "{point:?}");
            assert!(!shown.is_empty() || from == whole.len(), "{point:?}");
        }
    }

    #[test]
    fn a_large_single_line_of_tabs_or_wide_characters_resumes_near_any_row() {
        for line in ["\t".repeat(300_000), "\u{4e2d}".repeat(100_000)] {
            let raw = line.as_bytes();
            let widths = Widths {
                first: 70,
                rest: 70,
            };
            let points = resumes(Source::Raw(raw), widths);
            let gaps = points
                .windows(2)
                .map(|pair| pair[1].raw - pair[0].raw)
                .max()
                .unwrap();
            assert!(gaps <= CHUNK_BYTES + 4, "{gaps}");
            let (whole, result) = walked(Source::Raw(raw), widths, Resume::START);
            assert!(result.drawable);
            for row in [0, 1, result.rows / 2, result.rows - 3, result.rows - 1] {
                let from = resume_for(&points, row);
                let mut window = Vec::new();
                walk(
                    Source::Raw(raw),
                    widths,
                    from,
                    |_| {},
                    |shown, text| {
                        if shown >= row {
                            window.push((shown, text.to_owned()));
                        }
                        if shown + 1 >= row + 3 {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    },
                );
                let end = (row + 3).min(whole.len());
                assert_eq!(window, whole[row..end], "{row}");
                assert!(
                    from.raw + 2 * CHUNK_BYTES >= raw.len() * row / result.rows,
                    "{row} {from:?}"
                );
            }
        }
    }

    #[test]
    fn text_sources_wrap_like_raw_text_of_the_same_bytes() {
        let text = "12 unchanged lines \u{22ef}";
        let widths = Widths { first: 8, rest: 6 };
        assert_eq!(
            walked(Source::Text(text), widths, Resume::START).0,
            walked(Source::Raw(text.as_bytes()), widths, Resume::START).0
        );
        let (rows, result) = walked(Source::Text(""), widths, Resume::START);
        assert_eq!((rows, result.rows), (vec![(0, String::new())], 1));
    }
}
