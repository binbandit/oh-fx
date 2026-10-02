use std::collections::{HashMap, HashSet};

use crate::byte_trim::{trim_end, trim_start};
use crate::file_picker_path::at_path_end;
use crate::skill_contract::Skill;

const BLANK_LINE_BYTES: &[u8] = b" \t\r\n";
const LEFT_DOUBLE_QUOTE: &[u8] = "\u{201c}".as_bytes();
const RIGHT_DOUBLE_QUOTE: &[u8] = "\u{201d}".as_bytes();
const LEFT_SINGLE_QUOTE: &[u8] = "\u{2018}".as_bytes();
const RIGHT_SINGLE_QUOTE: &[u8] = "\u{2019}".as_bytes();
const NEGATING_ENDINGS: [&str; 22] = [
    "not",
    "don't",
    "without",
    "not use",
    "don't use",
    "not apply",
    "don't apply",
    "not invoke",
    "don't invoke",
    "not run",
    "don't run",
    "not activate",
    "don't activate",
    "not use the",
    "don't use the",
    "never",
    "never use",
    "never apply",
    "never invoke",
    "never run",
    "never activate",
    "never use the",
];
const INVOCATION_VERBS: [&str; 5] = ["use", "apply", "activate", "invoke", "run"];
const SKILL_MARKER: &[u8] = b" skill";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplicitSelection<'a> {
    Skill(usize),
    Ambiguous(&'a str),
}

pub fn collect_explicit_skill_selections<'a>(
    prompt: &str,
    skills: &'a [Skill],
) -> Vec<ExplicitSelection<'a>> {
    let text = prompt.as_bytes();
    let mut selections = Selections::default();
    let trimmed = trim_start(text, BLANK_LINE_BYTES);
    let natural_end = (0..text.len())
        .find(|&index| text[index] == b'$' || at_path_end(text, index).is_some())
        .unwrap_or(text.len());
    let natural = NaturalLanguageReference::parse(&text[..natural_end]);
    let mut name_counts: Option<HashMap<String, usize>> = None;
    for (index, skill) in skills.iter().enumerate() {
        let name = skill.name.as_bytes();
        let matches = matches_sigil_skill_at(trimmed, 0, name, b'/')
            || natural
                .as_ref()
                .is_some_and(|reference| reference.matches_skill_name(name));
        if !matches {
            continue;
        }
        let counts = name_counts.get_or_insert_with(|| case_folded_name_counts(skills));
        let unique = counts.get(&skill.name.to_ascii_lowercase()) == Some(&1);
        selections.push(if unique {
            ExplicitSelection::Skill(index)
        } else {
            ExplicitSelection::Ambiguous(&skill.name)
        });
    }
    let dollar_candidates = SigilCandidates::new(skills);
    for selection in dollar_selections(text, skills, &dollar_candidates) {
        selections.push(selection);
    }
    selections.ordered
}

#[derive(Default)]
struct Selections<'a> {
    ordered: Vec<ExplicitSelection<'a>>,
    skills: HashSet<usize>,
    ambiguous_names: HashSet<String>,
}

impl<'a> Selections<'a> {
    fn push(&mut self, selection: ExplicitSelection<'a>) {
        let fresh = match selection {
            ExplicitSelection::Skill(index) => self.skills.insert(index),
            ExplicitSelection::Ambiguous(name) => {
                self.ambiguous_names.insert(name.to_ascii_lowercase())
            }
        };
        if fresh {
            self.ordered.push(selection);
        }
    }
}

fn case_folded_name_counts(skills: &[Skill]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for skill in skills {
        *counts.entry(skill.name.to_ascii_lowercase()).or_insert(0) += 1;
    }
    counts
}

struct SigilCandidates {
    by_first_byte: HashMap<u8, Vec<usize>>,
}

impl SigilCandidates {
    fn new(skills: &[Skill]) -> Self {
        let mut by_first_byte: HashMap<u8, Vec<usize>> = HashMap::new();
        for (index, skill) in skills.iter().enumerate() {
            if let Some(first) = skill.name.as_bytes().first() {
                by_first_byte
                    .entry(first.to_ascii_lowercase())
                    .or_default()
                    .push(index);
            }
        }
        Self { by_first_byte }
    }

    fn starting_with(&self, byte: Option<&u8>) -> &[usize] {
        byte.and_then(|byte| self.by_first_byte.get(&byte.to_ascii_lowercase()))
            .map_or(&[], Vec::as_slice)
    }
}

#[derive(Clone, Copy)]
struct CodeSpan {
    byte: u8,
    len: usize,
    fenced: bool,
}

fn dollar_selections<'a>(
    text: &[u8],
    skills: &'a [Skill],
    candidates: &SigilCandidates,
) -> Vec<ExplicitSelection<'a>> {
    let mut selections = Vec::new();
    let mut quote: Option<u8> = None;
    let mut code: Option<CodeSpan> = None;
    let mut index = 0;
    while index < text.len() {
        let byte = text[index];
        if let Some(active) = code {
            if byte == active.byte {
                let end = code_run_end(text, index);
                if closes_code_span(text, index, end, active) {
                    code = None;
                }
                index = end;
            } else {
                index += 1;
            }
            continue;
        }
        if byte == b'\\' {
            index += if index + 1 < text.len() { 2 } else { 1 };
            continue;
        }
        if let Some(active) = quote {
            if byte == active {
                quote = None;
            }
            index += 1;
            continue;
        }
        if let Some(end) = at_path_end(text, index) {
            index = end;
            continue;
        }
        if byte == b'`' || byte == b'~' {
            let end = code_run_end(text, index);
            let fenced = end - index >= 3
                && fence_line_start(text, index)
                && (byte == b'~' || !current_line(&text[end..]).contains(&b'`'));
            if fenced || byte == b'`' {
                code = Some(CodeSpan {
                    byte,
                    len: end - index,
                    fenced,
                });
            }
            index = end;
            continue;
        }
        if byte == b'"'
            || (byte == b'\'' && (index == 0 || !text[index - 1].is_ascii_alphanumeric()))
        {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if let Some(close) = typographic_quote_close(&text[index..]) {
            let Some(next) = find(&text[index + 3..], close) else {
                break;
            };
            index += 3 + next + close.len();
            continue;
        }
        if byte != b'$' || explicit_reference_negated(&text[..index]) {
            index += 1;
            continue;
        }
        if let Some((selection, length)) = longest_dollar_match(text, index, skills, candidates) {
            selections.push(selection);
            index += length;
        }
        index += 1;
    }
    selections
}

fn longest_dollar_match<'a>(
    text: &[u8],
    index: usize,
    skills: &'a [Skill],
    candidates: &SigilCandidates,
) -> Option<(ExplicitSelection<'a>, usize)> {
    let mut found = None;
    let mut longest = 0;
    let mut ambiguous = false;
    for &skill_index in candidates.starting_with(text.get(index + 1)) {
        let name = skills[skill_index].name.as_bytes();
        if !matches_sigil_skill_at(text, index, name, b'$') || name.len() < longest {
            continue;
        }
        if name.len() > longest {
            found = Some(skill_index);
            longest = name.len();
            ambiguous = false;
        } else {
            ambiguous = true;
        }
    }
    let skill_index = found?;
    let selection = if ambiguous {
        ExplicitSelection::Ambiguous(&skills[skill_index].name)
    } else {
        ExplicitSelection::Skill(skill_index)
    };
    Some((selection, longest))
}

fn closes_code_span(text: &[u8], index: usize, end: usize, active: CodeSpan) -> bool {
    if !active.fenced {
        return end - index == active.len;
    }
    end - index >= active.len
        && fence_line_start(text, index)
        && current_line(&text[end..])
            .iter()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\r'))
}

fn current_line(text: &[u8]) -> &[u8] {
    let line_end = text
        .iter()
        .position(|&byte| byte == b'\n')
        .unwrap_or(text.len());
    &text[..line_end]
}

fn typographic_quote_close(text: &[u8]) -> Option<&'static [u8]> {
    if text.starts_with(LEFT_DOUBLE_QUOTE) {
        Some(RIGHT_DOUBLE_QUOTE)
    } else if text.starts_with(LEFT_SINGLE_QUOTE) {
        Some(RIGHT_SINGLE_QUOTE)
    } else {
        None
    }
}

fn code_run_end(text: &[u8], start: usize) -> usize {
    let mut end = start + 1;
    while end < text.len() && text[end] == text[start] {
        end += 1;
    }
    end
}

fn fence_line_start(text: &[u8], index: usize) -> bool {
    let mut start = index;
    while start > 0 && index - start < 3 && text[start - 1] == b' ' {
        start -= 1;
    }
    start == 0 || text[start - 1] == b'\n'
}

fn explicit_reference_negated(before: &[u8]) -> bool {
    let trimmed = trim_end(before, b" \t\r\n,:");
    NEGATING_ENDINGS.iter().any(|ending| {
        let ending = ending.as_bytes();
        let Some(start) = trimmed.len().checked_sub(ending.len()) else {
            return false;
        };
        (start == 0 || !trimmed[start - 1].is_ascii_alphanumeric())
            && trimmed[start..].eq_ignore_ascii_case(ending)
    })
}

struct NaturalLanguageReference {
    normalized_prompt: Vec<u8>,
    name_starts: Vec<usize>,
}

impl NaturalLanguageReference {
    fn parse(prompt: &[u8]) -> Option<Self> {
        let normalized_prompt = normalize_reference_text(natural_language_reference_start(prompt)?);
        let name_start = INVOCATION_VERBS.iter().find_map(|verb| {
            let verb = verb.as_bytes();
            (normalized_prompt.starts_with(verb)
                && normalized_prompt.get(verb.len()) == Some(&b' '))
            .then_some(verb.len() + 1)
        })?;
        let mut name_starts = vec![name_start];
        if normalized_prompt[name_start..].starts_with(b"the ") {
            name_starts.push(name_start + b"the ".len());
        }
        Some(Self {
            normalized_prompt,
            name_starts,
        })
    }

    fn matches_skill_name(&self, skill_name: &[u8]) -> bool {
        let name = normalize_reference_text(skill_name);
        if name.is_empty() {
            return false;
        }
        let prompt = self.normalized_prompt.as_slice();
        self.name_starts.iter().any(|&name_start| {
            let marker_start = name_start + name.len();
            let reference_end = marker_start + SKILL_MARKER.len();
            prompt.get(name_start..marker_start) == Some(name.as_slice())
                && prompt.get(marker_start..reference_end) == Some(SKILL_MARKER)
                && prompt.get(reference_end).is_none_or(|&byte| byte == b' ')
        })
    }
}

fn natural_language_reference_start(prompt: &[u8]) -> Option<&[u8]> {
    let mut text = trim_start(prompt, BLANK_LINE_BYTES);
    let please = b"please";
    if text.len() >= please.len()
        && text[..please.len()].eq_ignore_ascii_case(please)
        && text
            .get(please.len())
            .is_none_or(|byte| !byte.is_ascii_alphanumeric())
    {
        text = trim_start(&text[please.len()..], b" \t\r\n,:");
    }
    let quoted = matches!(text.first(), None | Some(b'"' | b'\'' | b'`'))
        || text.starts_with(LEFT_DOUBLE_QUOTE)
        || text.starts_with(LEFT_SINGLE_QUOTE);
    (!quoted).then_some(text)
}

fn matches_sigil_skill_at(text: &[u8], index: usize, skill_name: &[u8], sigil: u8) -> bool {
    if text.get(index) != Some(&sigil) {
        return false;
    }
    let name_start = index + 1;
    let end = name_start + skill_name.len();
    text.get(name_start..end)
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(skill_name))
        && text
            .get(end)
            .is_none_or(|&byte| !is_skill_name_continuation(byte))
}

fn is_skill_name_continuation(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn normalize_reference_text(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut previous_was_space = true;
    for &byte in text {
        if byte.is_ascii_alphanumeric() {
            out.push(byte.to_ascii_lowercase());
            previous_was_space = false;
        } else if !previous_was_space {
            out.push(b' ');
            previous_was_space = true;
        }
    }
    if out.last() == Some(&b' ') {
        out.pop();
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests;
