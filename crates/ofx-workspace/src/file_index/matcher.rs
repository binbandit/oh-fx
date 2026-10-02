use std::ops::Range;

use ofx_text::display_unit_at;

use super::{Generation, MAX_PATH_LEN};
use crate::unicode_simple_fold::fold;

const NON_ASCII_MASK: u32 = 1 << 31;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SubsequenceFacts {
    boundary_matches: usize,
    prefix: bool,
    first_position: usize,
    longest_run: usize,
    consecutive_matches: usize,
    gaps: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MatchScore {
    exact_fit: bool,
    basename_fit: bool,
    facts: SubsequenceFacts,
}

impl MatchScore {
    fn new(exact_fit: bool, basename_fit: bool, facts: SubsequenceFacts) -> Self {
        Self {
            exact_fit,
            basename_fit,
            facts,
        }
    }

    fn better_than(&self, other: &Self) -> bool {
        let (left, right) = (&self.facts, &other.facts);
        if self.exact_fit != other.exact_fit {
            return self.exact_fit;
        }
        if self.basename_fit != other.basename_fit {
            return self.basename_fit;
        }
        if left.boundary_matches != right.boundary_matches {
            return left.boundary_matches > right.boundary_matches;
        }
        if left.prefix != right.prefix {
            return left.prefix;
        }
        if left.first_position != right.first_position {
            return left.first_position < right.first_position;
        }
        if left.longest_run != right.longest_run {
            return left.longest_run > right.longest_run;
        }
        if left.consecutive_matches != right.consecutive_matches {
            return left.consecutive_matches > right.consecutive_matches;
        }
        left.gaps < right.gaps
    }
}

pub(super) struct PreparedQuery {
    folded: Vec<char>,
    ascii: Option<Vec<u8>>,
    mask: u32,
}

impl PreparedQuery {
    pub(super) fn new(query: &str) -> Option<Self> {
        if query.len() > MAX_PATH_LEN {
            return None;
        }
        let folded: Vec<char> = query.chars().map(fold).collect();
        let ascii = folded
            .iter()
            .map(|scalar| u8::try_from(*scalar).ok().filter(u8::is_ascii))
            .collect::<Option<Vec<u8>>>();
        let mask = ascii.as_deref().map_or(0, alpha_mask);
        Some(Self {
            folded,
            ascii,
            mask,
        })
    }

    fn score(
        &self,
        path: &str,
        lower: &[u8],
        basename_offset: usize,
        path_mask: u32,
    ) -> Option<MatchScore> {
        if path_mask & NON_ASCII_MASK == 0 {
            let ascii = self.ascii.as_deref()?;
            if path_mask & self.mask != self.mask {
                return None;
            }
            return score_ascii_match(path, lower, basename_offset, ascii);
        }
        score_folded_match(path, basename_offset, &self.folded)
    }
}

pub(crate) struct NameQuery {
    prepared: PreparedQuery,
}

impl NameQuery {
    pub(crate) fn new(query: &str) -> Option<Self> {
        PreparedQuery::new(query).map(|prepared| Self { prepared })
    }

    pub(crate) fn score(&self, name: &str) -> Option<MatchScore> {
        if name.is_empty() || name.len() > MAX_PATH_LEN {
            return None;
        }
        if self.prepared.folded.is_empty() {
            return Some(MatchScore::default());
        }
        let lower: Vec<u8> = name.bytes().map(|byte| byte.to_ascii_lowercase()).collect();
        self.prepared.score(name, &lower, 0, path_mask(name))
    }

    pub(crate) fn better(
        &self,
        left: &MatchScore,
        left_path: &str,
        right: &MatchScore,
        right_path: &str,
    ) -> bool {
        if !self.prepared.folded.is_empty() {
            if left.better_than(right) {
                return true;
            }
            if right.better_than(left) {
                return false;
            }
            if left_path.len() != right_path.len() {
                return left_path.len() < right_path.len();
            }
        }
        left_path < right_path
    }

    pub(crate) fn match_spans(&self, name: &str) -> Option<Vec<Range<usize>>> {
        if self.prepared.folded.is_empty() {
            return Some(Vec::new());
        }
        match_spans(name, 0, &self.prepared)
    }
}

pub(super) fn path_mask(path: &str) -> u32 {
    let mask = alpha_mask(path.as_bytes());
    if path.is_ascii() {
        mask
    } else {
        mask | NON_ASCII_MASK
    }
}

fn alpha_mask(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .map(u8::to_ascii_lowercase)
        .filter(u8::is_ascii_lowercase)
        .fold(0, |mask, byte| mask | 1 << (byte - b'a'))
}

pub(super) fn rank_top(
    generation: &Generation,
    total: usize,
    query: &PreparedQuery,
    cap: usize,
) -> Vec<usize> {
    if cap == 0 {
        return Vec::new();
    }
    let better = |left: &(MatchScore, usize), right: &(MatchScore, usize)| {
        ranked_better(generation, left, right)
    };
    let mut ranked: Vec<(MatchScore, usize)> = Vec::with_capacity(cap);
    let mut worst = 0;
    for index in 0..total {
        let Some(score) = query.score(
            generation.path_at(index),
            generation.lower_path_at(index),
            generation.basename_offset(index),
            generation.masks[index],
        ) else {
            continue;
        };
        let candidate = (score, index);
        if ranked.len() < cap {
            ranked.push(candidate);
            if ranked.len() == cap {
                worst = worst_slot(&ranked, &better);
            }
        } else if better(&candidate, &ranked[worst]) {
            ranked[worst] = candidate;
            worst = worst_slot(&ranked, &better);
        }
    }
    insertion_sort(&mut ranked, &better);
    ranked.into_iter().map(|(_, index)| index).collect()
}

fn insertion_sort<T>(items: &mut [T], better: &impl Fn(&T, &T) -> bool) {
    for end in 1..items.len() {
        let mut position = end;
        while let Some([left, right]) = position
            .checked_sub(1)
            .and_then(|start| items.get_mut(start..=position))
        {
            if !better(right, left) {
                break;
            }
            std::mem::swap(left, right);
            position -= 1;
        }
    }
}

fn ranked_better(
    generation: &Generation,
    left: &(MatchScore, usize),
    right: &(MatchScore, usize),
) -> bool {
    if left.0.better_than(&right.0) {
        return true;
    }
    if right.0.better_than(&left.0) {
        return false;
    }
    let left_path = generation.path_at(left.1);
    let right_path = generation.path_at(right.1);
    if left_path.len() != right_path.len() {
        return left_path.len() < right_path.len();
    }
    left_path < right_path
}

fn worst_slot<T>(ranked: &[T], better: &impl Fn(&T, &T) -> bool) -> usize {
    let mut worst = 0;
    for (index, candidate) in ranked.iter().enumerate().skip(1) {
        if better(&ranked[worst], candidate) {
            worst = index;
        }
    }
    worst
}

fn score_ascii_match(
    path: &str,
    lower: &[u8],
    basename_offset: usize,
    query: &[u8],
) -> Option<MatchScore> {
    if query.is_empty() || path.is_empty() {
        return None;
    }
    let exact_fit = lower == query || &lower[basename_offset..] == query;
    let matches = |position: usize, query_index: usize| lower[position] == query[query_index];
    let facts_from = |range_start: usize| {
        subsequence_facts(
            path.as_bytes(),
            range_start,
            query.len(),
            range_start..lower.len(),
            |position| position,
            matches,
        )
    };
    if let Some(facts) = facts_from(basename_offset) {
        return Some(MatchScore::new(exact_fit, true, facts));
    }
    let facts = facts_from(0)?;
    Some(MatchScore::new(exact_fit, false, facts))
}

struct FoldedPath {
    scalars: Vec<char>,
    byte_offsets: Vec<usize>,
    basename_scalar: usize,
}

impl FoldedPath {
    fn decode(path: &str, basename_offset: usize) -> Option<Self> {
        let mut scalars = Vec::new();
        let mut byte_offsets = Vec::new();
        let mut basename_scalar = None;
        for (offset, scalar) in path.char_indices() {
            if offset == basename_offset {
                basename_scalar = Some(scalars.len());
            }
            byte_offsets.push(offset);
            scalars.push(fold(scalar));
        }
        if basename_offset == path.len() {
            basename_scalar = Some(scalars.len());
        }
        byte_offsets.push(path.len());
        Some(Self {
            scalars,
            byte_offsets,
            basename_scalar: basename_scalar?,
        })
    }
}

fn score_folded_match(path: &str, basename_offset: usize, query: &[char]) -> Option<MatchScore> {
    if query.is_empty() || path.is_empty() {
        return None;
    }
    let folded = FoldedPath::decode(path, basename_offset)?;
    let scalars = &folded.scalars;
    let exact_fit = scalars == query || &scalars[folded.basename_scalar..] == query;
    let facts_from = |range_start: usize| {
        if query.len() > scalars.len() - range_start {
            return None;
        }
        subsequence_facts(
            path.as_bytes(),
            range_start,
            query.len(),
            range_start..scalars.len(),
            |position| folded.byte_offsets[position],
            |position, query_index| scalars[position] == query[query_index],
        )
    };
    if let Some(facts) = facts_from(folded.basename_scalar) {
        return Some(MatchScore::new(exact_fit, true, facts));
    }
    let facts = facts_from(0)?;
    Some(MatchScore::new(exact_fit, false, facts))
}

fn subsequence_facts(
    path: &[u8],
    range_start: usize,
    query_len: usize,
    positions: impl Iterator<Item = usize>,
    byte_at: impl Fn(usize) -> usize,
    matches: impl Fn(usize, usize) -> bool,
) -> Option<SubsequenceFacts> {
    let mut facts = SubsequenceFacts::default();
    let mut query_index = 0;
    let mut previous = 0;
    let mut run = 0;
    for position in positions {
        if query_index == query_len {
            break;
        }
        if !matches(position, query_index) {
            continue;
        }
        if query_index == 0 {
            facts.first_position = position - range_start;
            facts.prefix = position == range_start;
            run = 1;
        } else if position == previous + 1 {
            facts.consecutive_matches += 1;
            run += 1;
        } else {
            facts.gaps += position - previous - 1;
            run = 1;
        }
        facts.longest_run = facts.longest_run.max(run);
        facts.boundary_matches += usize::from(is_match_boundary(path, byte_at(position)));
        previous = position;
        query_index += 1;
    }
    (query_index == query_len).then_some(facts)
}

fn is_match_boundary(path: &[u8], byte_index: usize) -> bool {
    if byte_index == 0 {
        return true;
    }
    let previous = path[byte_index - 1];
    if matches!(previous, b'/' | b'-' | b'_' | b'.' | b' ') {
        return true;
    }
    path.get(byte_index)
        .is_some_and(|current| previous.is_ascii_lowercase() && current.is_ascii_uppercase())
}

pub(super) fn match_spans(
    path: &str,
    basename_offset: usize,
    query: &PreparedQuery,
) -> Option<Vec<Range<usize>>> {
    let folded = FoldedPath::decode(path, basename_offset)?;
    let offsets = collect_match_offsets(&folded, folded.basename_scalar, &query.folded)
        .or_else(|| collect_match_offsets(&folded, 0, &query.folded))?;
    spans_from_matched_offsets(path, &offsets)
}

fn collect_match_offsets(
    folded: &FoldedPath,
    range_start: usize,
    query: &[char],
) -> Option<Vec<usize>> {
    if query.len() > folded.scalars.len() - range_start {
        return None;
    }
    let mut offsets = Vec::with_capacity(query.len());
    for position in range_start..folded.scalars.len() {
        if offsets.len() == query.len() {
            break;
        }
        if folded.scalars[position] == query[offsets.len()] {
            offsets.push(folded.byte_offsets[position]);
        }
    }
    (offsets.len() == query.len()).then_some(offsets)
}

fn spans_from_matched_offsets(path: &str, offsets: &[usize]) -> Option<Vec<Range<usize>>> {
    let mut spans: Vec<Range<usize>> = Vec::new();
    let mut matched = 0;
    let mut cursor = 0;
    while cursor < path.len() && matched < offsets.len() {
        let cluster_start = cursor;
        let first = display_unit_at(path, cursor);
        if first.byte_len == 0 {
            return None;
        }
        cursor += first.byte_len;
        while cursor < path.len() {
            let continuation = display_unit_at(path, cursor);
            if continuation.byte_len == 0 {
                return None;
            }
            if continuation.cell_width != 0 {
                break;
            }
            cursor += continuation.byte_len;
        }
        let mut cluster_matched = false;
        while matched < offsets.len() && offsets[matched] < cursor {
            if offsets[matched] < cluster_start {
                return None;
            }
            cluster_matched = true;
            matched += 1;
        }
        if !cluster_matched {
            continue;
        }
        match spans.last_mut() {
            Some(last) if last.end == cluster_start => last.end = cursor,
            _ => spans.push(cluster_start..cursor),
        }
    }
    (matched == offsets.len()).then_some(spans)
}
