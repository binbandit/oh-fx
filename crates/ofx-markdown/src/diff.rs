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
    pub text: &'a [u8],
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
            return Rows::Notice(Some(ReviewLine {
                op: ReviewOp::Notice,
                text: NO_CONTENT_CHANGES,
            }));
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
        let line = |op: ReviewOp| move |text: &'a [u8]| ReviewLine { op, text };
        let elision = |count: usize| {
            (count > 0).then_some(ReviewLine {
                op: ReviewOp::Elision,
                text: b"",
            })
        };
        elision(prefix_elision)
            .into_iter()
            .chain(
                text_lines(self.old_text)
                    .skip(prefix_elision)
                    .take(prefix_context)
                    .map(line(ReviewOp::Context)),
            )
            .chain(
                text_lines(self.old_text)
                    .skip(self.prefix_count)
                    .take(self.old_middle_count)
                    .map(line(ReviewOp::Deletion)),
            )
            .chain(
                text_lines(self.new_text)
                    .skip(self.prefix_count)
                    .take(self.new_middle_count)
                    .map(line(ReviewOp::Addition)),
            )
            .chain(self.old_marker.map(line(ReviewOp::Deletion)))
            .chain(self.new_marker.map(line(ReviewOp::Addition)))
            .chain(
                text_lines(self.old_text)
                    .skip(self.prefix_count + self.old_middle_count)
                    .take(suffix_context)
                    .map(line(ReviewOp::Context)),
            )
            .chain(elision(suffix_elision))
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
                rows.push(ReviewLine {
                    op: ReviewOp::Addition,
                    text: line.text,
                });
                index += 1;
            }
            LineOp::Remove => {
                rows.push(ReviewLine {
                    op: ReviewOp::Deletion,
                    text: line.text,
                });
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
        rows.extend(range.iter().map(|line| ReviewLine {
            op: ReviewOp::Context,
            text: line.text,
        }));
    };
    let elision = |count: usize, rows: &mut Vec<ReviewLine<'a>>| {
        if count > 0 {
            rows.push(ReviewLine {
                op: ReviewOp::Elision,
                text: b"",
            });
        }
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
                text: old_lines[old_cursor - 1],
            });
            old_cursor -= 1;
            new_cursor -= 1;
        } else if new_cursor > 0
            && (old_cursor == 0
                || table[old_cursor * stride + new_cursor - 1]
                    >= table[(old_cursor - 1) * stride + new_cursor])
        {
            result.push(DiffLine {
                op: LineOp::Add,
                text: new_lines[new_cursor - 1],
            });
            new_cursor -= 1;
        } else {
            result.push(DiffLine {
                op: LineOp::Remove,
                text: old_lines[old_cursor - 1],
            });
            old_cursor -= 1;
        }
    }
    result.reverse();
    result
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
