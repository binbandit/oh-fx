use std::collections::HashMap;

const REVIEW_CONTEXT_LINES: usize = 5;
const CANONICAL_MAX_INDEXED_LINES: usize = 16_384;
const CANONICAL_MAX_MATRIX_CELLS: usize = 1_000_000;
const TRAILING_NEWLINE_ADDED_MARKER: &[u8] = b"(trailing newline added)";
const TRAILING_NEWLINE_REMOVED_MARKER: &[u8] = b"(trailing newline removed)";
const NO_CONTENT_CHANGES: &[u8] = b"No content changes";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewOp {
    Context,
    Addition,
    Deletion,
    Elision,
    Notice,
}

impl ReviewOp {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Addition => "addition",
            Self::Deletion => "deletion",
            Self::Elision => "elision",
            Self::Notice => "notice",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewLine<'a> {
    pub op: ReviewOp,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: &'a [u8],
    pub elision_count: usize,
}

impl<'a> ReviewLine<'a> {
    fn new(op: ReviewOp, old_line: Option<u32>, new_line: Option<u32>, text: &'a [u8]) -> Self {
        Self {
            op,
            old_line,
            new_line,
            text,
            elision_count: 0,
        }
    }

    fn elision(count: usize) -> Option<Self> {
        (count > 0).then_some(Self {
            op: ReviewOp::Elision,
            old_line: None,
            new_line: None,
            text: b"",
            elision_count: count,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineOp {
    Equal,
    Add,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiffLine<'a> {
    op: LineOp,
    old_line: Option<u32>,
    new_line: Option<u32>,
    text: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReview<'a> {
    mode: Mode<'a>,
    additions: usize,
    deletions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode<'a> {
    Computed(Vec<DiffLine<'a>>),
    Fallback(Fallback<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fallback<'a> {
    old_text: &'a [u8],
    new_text: &'a [u8],
    prefix_count: usize,
    suffix_count: usize,
    old_middle_count: usize,
    new_middle_count: usize,
    old_marker: Option<&'static [u8]>,
    new_marker: Option<&'static [u8]>,
}

impl<'a> FileReview<'a> {
    pub fn new(old_text: &'a [u8], new_text: &'a [u8]) -> Self {
        let old_marker = trailing_newline_marker(old_text, new_text, true);
        let new_marker = trailing_newline_marker(old_text, new_text, false);
        let old_line_count = count_text_lines(old_text) + usize::from(old_marker.is_some());
        let new_line_count = count_text_lines(new_text) + usize::from(new_marker.is_some());
        let indexed_lines = old_line_count.saturating_add(new_line_count);
        let matrix_cells = (old_line_count + 1).saturating_mul(new_line_count + 1);
        if indexed_lines <= CANONICAL_MAX_INDEXED_LINES
            && matrix_cells <= CANONICAL_MAX_MATRIX_CELLS
        {
            let lines = compute(old_text, new_text, old_marker, new_marker);
            let additions = lines.iter().filter(|line| line.op == LineOp::Add).count();
            let deletions = lines
                .iter()
                .filter(|line| line.op == LineOp::Remove)
                .count();
            return Self {
                mode: Mode::Computed(lines),
                additions,
                deletions,
            };
        }
        let fallback = Fallback::new(old_text, new_text, old_marker, new_marker);
        Self {
            additions: fallback.new_middle_count + usize::from(new_marker.is_some()),
            deletions: fallback.old_middle_count + usize::from(old_marker.is_some()),
            mode: Mode::Fallback(fallback),
        }
    }

    pub fn additions(&self) -> usize {
        self.additions
    }

    pub fn deletions(&self) -> usize {
        self.deletions
    }

    pub fn rows(&self) -> impl Iterator<Item = ReviewLine<'a>> + '_ {
        if self.additions + self.deletions == 0 {
            return Rows::Notice(Some(ReviewLine::new(
                ReviewOp::Notice,
                None,
                None,
                NO_CONTENT_CHANGES,
            )));
        }
        match &self.mode {
            Mode::Computed(source) => Rows::Computed(computed_rows(source).into_iter()),
            Mode::Fallback(source) => Rows::Fallback(source.rows()),
        }
    }
}

enum Rows<'a, C, F> {
    Notice(Option<ReviewLine<'a>>),
    Computed(C),
    Fallback(F),
}

impl<'a, C, F> Iterator for Rows<'a, C, F>
where
    C: Iterator<Item = ReviewLine<'a>>,
    F: Iterator<Item = ReviewLine<'a>>,
{
    type Item = ReviewLine<'a>;

    fn next(&mut self) -> Option<ReviewLine<'a>> {
        match self {
            Self::Notice(line) => line.take(),
            Self::Computed(rows) => rows.next(),
            Self::Fallback(rows) => rows.next(),
        }
    }
}

impl<'a> Fallback<'a> {
    fn new(
        old_text: &'a [u8],
        new_text: &'a [u8],
        old_marker: Option<&'static [u8]>,
        new_marker: Option<&'static [u8]>,
    ) -> Self {
        let old_count = count_text_lines(old_text);
        let new_count = count_text_lines(new_text);
        let comparable = old_count.min(new_count);
        let prefix_count = text_lines(old_text)
            .zip(text_lines(new_text))
            .take_while(|(old, new)| old == new)
            .count();
        let suffix_count = reversed_text_lines(old_text)
            .zip(reversed_text_lines(new_text))
            .take(comparable - prefix_count)
            .take_while(|(old, new)| old == new)
            .count();
        Self {
            old_text,
            new_text,
            prefix_count,
            suffix_count,
            old_middle_count: old_count - prefix_count - suffix_count,
            new_middle_count: new_count - prefix_count - suffix_count,
            old_marker,
            new_marker,
        }
    }

    fn rows(&self) -> impl Iterator<Item = ReviewLine<'a>> + '_ {
        let prefix_context = self.prefix_count.min(REVIEW_CONTEXT_LINES);
        let prefix_elision = self.prefix_count - prefix_context;
        let suffix_context = self.suffix_count.min(REVIEW_CONTEXT_LINES);
        let suffix_elision = self.suffix_count - suffix_context;
        let old_suffix = self.prefix_count + self.old_middle_count;
        let new_suffix = self.prefix_count + self.new_middle_count;
        let lines = |text: &'a [u8], start: usize, count: usize| {
            text_lines(text)
                .skip(start)
                .take(count)
                .zip(start..)
                .map(|(line, index)| (line, line_number(index)))
        };
        ReviewLine::elision(prefix_elision)
            .into_iter()
            .chain(
                lines(self.old_text, prefix_elision, prefix_context)
                    .map(|(text, number)| ReviewLine::new(ReviewOp::Context, number, number, text)),
            )
            .chain(
                lines(self.old_text, self.prefix_count, self.old_middle_count)
                    .map(|(text, number)| ReviewLine::new(ReviewOp::Deletion, number, None, text)),
            )
            .chain(
                lines(self.new_text, self.prefix_count, self.new_middle_count)
                    .map(|(text, number)| ReviewLine::new(ReviewOp::Addition, None, number, text)),
            )
            .chain(self.old_marker.map(|marker| {
                let number = line_number(count_text_lines(self.old_text));
                ReviewLine::new(ReviewOp::Deletion, number, None, marker)
            }))
            .chain(self.new_marker.map(|marker| {
                let number = line_number(count_text_lines(self.new_text));
                ReviewLine::new(ReviewOp::Addition, None, number, marker)
            }))
            .chain(
                lines(self.old_text, old_suffix, suffix_context)
                    .zip(new_suffix..)
                    .map(|((text, old_number), new_index)| {
                        ReviewLine::new(ReviewOp::Context, old_number, line_number(new_index), text)
                    }),
            )
            .chain(ReviewLine::elision(suffix_elision))
    }
}

fn computed_rows<'a>(source: &[DiffLine<'a>]) -> Vec<ReviewLine<'a>> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < source.len() {
        let line = source[index];
        match line.op {
            LineOp::Equal => {
                let run_start = index;
                while index < source.len() && source[index].op == LineOp::Equal {
                    index += 1;
                }
                append_equal_run(source, run_start, index, &mut rows);
            }
            LineOp::Add => {
                rows.push(ReviewLine::new(
                    ReviewOp::Addition,
                    None,
                    line.new_line,
                    line.text,
                ));
                index += 1;
            }
            LineOp::Remove => {
                rows.push(ReviewLine::new(
                    ReviewOp::Deletion,
                    line.old_line,
                    None,
                    line.text,
                ));
                index += 1;
            }
        }
    }
    rows
}

fn append_equal_run<'a>(
    source: &[DiffLine<'a>],
    run_start: usize,
    run_end: usize,
    rows: &mut Vec<ReviewLine<'a>>,
) {
    let context = |range: &[DiffLine<'a>], rows: &mut Vec<ReviewLine<'a>>| {
        rows.extend(range.iter().map(|line| {
            ReviewLine::new(ReviewOp::Context, line.old_line, line.new_line, line.text)
        }));
    };
    let elision = |count: usize, rows: &mut Vec<ReviewLine<'a>>| {
        rows.extend(ReviewLine::elision(count));
    };
    if run_start == 0 {
        let context_start = run_start.max(run_end.saturating_sub(REVIEW_CONTEXT_LINES));
        elision(context_start - run_start, rows);
        context(&source[context_start..run_end], rows);
        return;
    }
    if run_end == source.len() {
        let context_end = run_end.min(run_start + REVIEW_CONTEXT_LINES);
        context(&source[run_start..context_end], rows);
        elision(run_end - context_end, rows);
        return;
    }
    let first_context_end = run_end.min(run_start + REVIEW_CONTEXT_LINES);
    let last_context_start = first_context_end.max(run_end.saturating_sub(REVIEW_CONTEXT_LINES));
    context(&source[run_start..first_context_end], rows);
    elision(last_context_start - first_context_end, rows);
    context(&source[last_context_start..run_end], rows);
}

fn compute<'a>(
    old_text: &'a [u8],
    new_text: &'a [u8],
    old_marker: Option<&'static [u8]>,
    new_marker: Option<&'static [u8]>,
) -> Vec<DiffLine<'a>> {
    let old_lines: Vec<&[u8]> = text_lines(old_text).chain(old_marker).collect();
    let new_lines: Vec<&[u8]> = text_lines(new_text).chain(new_marker).collect();
    let table = LcsTable::new(&old_lines, &new_lines);
    let mut result = Vec::with_capacity(old_lines.len().max(new_lines.len()));
    let mut old_cursor = old_lines.len();
    let mut new_cursor = new_lines.len();
    while old_cursor > 0 || new_cursor > 0 {
        if old_cursor > 0
            && new_cursor > 0
            && old_lines[old_cursor - 1] == new_lines[new_cursor - 1]
        {
            result.push(DiffLine {
                op: LineOp::Equal,
                old_line: line_number(old_cursor - 1),
                new_line: line_number(new_cursor - 1),
                text: old_lines[old_cursor - 1],
            });
            old_cursor -= 1;
            new_cursor -= 1;
        } else if new_cursor > 0
            && (old_cursor == 0
                || table.length(old_cursor, new_cursor - 1)
                    >= table.length(old_cursor - 1, new_cursor))
        {
            result.push(DiffLine {
                op: LineOp::Add,
                old_line: None,
                new_line: line_number(new_cursor - 1),
                text: new_lines[new_cursor - 1],
            });
            new_cursor -= 1;
        } else {
            result.push(DiffLine {
                op: LineOp::Remove,
                old_line: line_number(old_cursor - 1),
                new_line: None,
                text: old_lines[old_cursor - 1],
            });
            old_cursor -= 1;
        }
    }
    result.reverse();
    result
}

struct LcsTable {
    prefix: usize,
    stride: usize,
    lengths: Vec<u16>,
}

impl LcsTable {
    fn new(old_lines: &[&[u8]], new_lines: &[&[u8]]) -> Self {
        let prefix = old_lines
            .iter()
            .zip(new_lines)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = old_lines[prefix..]
            .iter()
            .rev()
            .zip(new_lines[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let (old_ids, new_ids) = line_ids(
            &old_lines[prefix..old_lines.len() - suffix],
            &new_lines[prefix..new_lines.len() - suffix],
        );
        let stride = new_ids.len() + 1;
        let mut lengths = vec![0_u16; (old_ids.len() + 1) * stride];
        for (old_index, old_id) in old_ids.iter().enumerate() {
            let (above, current) = lengths[old_index * stride..].split_at_mut(stride);
            let mut left = 0;
            let cells = current[1..].iter_mut().zip(&new_ids);
            for ((cell, new_id), (diagonal, up)) in cells.zip(above.iter().zip(&above[1..])) {
                left = if old_id == new_id {
                    diagonal + 1
                } else {
                    left.max(*up)
                };
                *cell = left;
            }
        }
        Self {
            prefix,
            stride,
            lengths,
        }
    }

    fn length(&self, old_count: usize, new_count: usize) -> usize {
        if old_count <= self.prefix || new_count <= self.prefix {
            return old_count.min(new_count);
        }
        let row = old_count - self.prefix;
        let column = new_count - self.prefix;
        self.prefix + usize::from(self.lengths[row * self.stride + column])
    }
}

fn line_ids<'a>(old_lines: &[&'a [u8]], new_lines: &[&'a [u8]]) -> (Vec<u32>, Vec<u32>) {
    let mut ids: HashMap<&'a [u8], u32> = HashMap::with_capacity(old_lines.len() + new_lines.len());
    let mut id = |line: &&'a [u8]| {
        let next = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        *ids.entry(*line).or_insert(next)
    };
    let old_ids = old_lines.iter().map(&mut id).collect();
    let new_ids = new_lines.iter().map(&mut id).collect();
    (old_ids, new_ids)
}

fn line_number(index: usize) -> Option<u32> {
    u32::try_from(index + 1).ok()
}

fn text_lines(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    line_body(text)
        .into_iter()
        .flat_map(|body| body.split(|byte| *byte == b'\n'))
}

fn reversed_text_lines(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    line_body(text)
        .into_iter()
        .flat_map(|body| body.rsplit(|byte| *byte == b'\n'))
}

fn line_body(text: &[u8]) -> Option<&[u8]> {
    (!text.is_empty()).then(|| text.strip_suffix(b"\n").unwrap_or(text))
}

fn count_text_lines(text: &[u8]) -> usize {
    if text.is_empty() {
        return 0;
    }
    memchr::memchr_iter(b'\n', text).count() + usize::from(text.last() != Some(&b'\n'))
}

fn trailing_newline_marker(
    old_text: &[u8],
    new_text: &[u8],
    old_side: bool,
) -> Option<&'static [u8]> {
    if old_text.is_empty() || new_text.is_empty() {
        return None;
    }
    let old_has_newline = old_text.ends_with(b"\n");
    let new_has_newline = new_text.ends_with(b"\n");
    if old_has_newline == new_has_newline {
        return None;
    }
    if old_side && old_has_newline {
        return Some(TRAILING_NEWLINE_REMOVED_MARKER);
    }
    if !old_side && new_has_newline {
        return Some(TRAILING_NEWLINE_ADDED_MARKER);
    }
    None
}

#[cfg(test)]
mod tests;
