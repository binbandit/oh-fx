use ofx_text::{contains_ignore_case, is_posix_space};

use super::checkpoint::{CHECK_MARK, Entry, Highest, replaced_ids, was_used};
use super::ledger::{Note, Written};

const MAX_VALUES: usize = 16;
const CLOSING_PUNCTUATION: [char; 7] = [' ', ',', '.', ';', ':', '!', '?'];
const SUCCESS_WORDS: [&str; 11] = [
    "pass",
    "passed",
    "passes",
    "passing",
    "succeeded",
    "success",
    "successful",
    "successfully",
    "works",
    "worked",
    "green",
];
const FAILURE_WORDS: [&str; 19] = [
    "fail", "failed", "fails", "failing", "failure", "error", "errors", "broke", "broken", "crash",
    "crashed", "not", "no", "timeout", "rejected", "denied", "missing", "exit", "nonzero",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) number: usize,
    pub(crate) text: String,
    pub(crate) failed: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) marked: usize,
    pub(crate) no_source: usize,
    pub(crate) missing_ids: usize,
    pub(crate) unfound_values: usize,
    pub(crate) bad_replaces: usize,
    pub(crate) unquoted: usize,
    pub(crate) failed_as_success: usize,
}

pub(crate) struct Sources<'a> {
    pub(crate) turn_count: usize,
    pub(crate) tool_count: usize,
    pub(crate) turns: &'a [Record],
    pub(crate) tools: &'a [Record],
    pub(crate) users: &'a [&'a str],
    pub(crate) kept: &'a [&'a str],
    pub(crate) highest: Highest,
}

pub(crate) fn check(
    written: Written,
    earlier: &[Entry],
    sources: &Sources<'_>,
    counts: &mut Counts,
) -> Written {
    let users: Vec<String> = sources.users.iter().map(|user| normalized(user)).collect();
    let all: Vec<&str> = sources
        .turns
        .iter()
        .chain(sources.tools)
        .map(|record| record.text.as_str())
        .chain(sources.users.iter().copied())
        .chain(sources.kept.iter().copied())
        .collect();

    let works = written
        .works
        .iter()
        .map(|note| {
            let mut problems = Problems::default();
            check_citations(&mut problems, &note.text, sources, counts);
            check_values(&mut problems, &note.text, &all, counts);
            Note {
                number: note.number,
                text: problems.mark(&note.text, counts),
            }
        })
        .collect();

    let tools = written
        .tools
        .iter()
        .map(|note| {
            let mut problems = Problems::default();
            check_citations(&mut problems, &note.text, sources, counts);
            check_values(&mut problems, &note.text, &all, counts);
            let cited = citations(&note.text);
            let shared = cited
                .first()
                .is_some_and(|first| first.first == note.number && first.last > note.number);
            if !shared {
                check_failed_call(
                    &mut problems,
                    &note.text,
                    note.number,
                    sources.tools,
                    counts,
                );
            }
            Note {
                number: note.number,
                text: problems.mark(&note.text, counts),
            }
        })
        .collect();

    let entries = written
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut problems = Problems::default();
            let cited = citations(&entry.text);
            let names_a_source =
                !cited.is_empty() || contains_ignore_case(&entry.text, "turn in progress");
            let kind = entry.id.as_bytes().first().copied();
            if !names_a_source {
                counts.no_source += 1;
                problems.add("no source");
            }
            check_citations(&mut problems, &entry.text, sources, counts);
            if kind != Some(b'R')
                && let Some(number) = only_tool(&cited)
            {
                check_failed_call(&mut problems, &entry.text, number, sources.tools, counts);
            }
            if kind == Some(b'R') {
                check_quote(&mut problems, &entry.text, &users, counts);
            } else if names_this_compaction(&cited, sources) {
                check_values(&mut problems, &entry.text, &all, counts);
            }
            for id in replaced_ids(&entry.text) {
                let exists = has_entry(earlier, id)
                    || has_entry(&written.entries[..index], id)
                    || was_used(id, &sources.highest);
                if !exists {
                    counts.bad_replaces += 1;
                    problems.add(&format!("replaces {id}, which does not exist"));
                }
            }
            Entry {
                id: entry.id.clone(),
                text: problems.mark(&entry.text, counts),
            }
        })
        .collect();

    Written {
        works,
        tools,
        entries,
        ..written
    }
}

#[derive(Default)]
struct Problems {
    text: String,
}

impl Problems {
    fn add(&mut self, problem: &str) {
        if !self.text.is_empty() {
            self.text.push_str("; ");
        }
        self.text.push_str(problem);
    }

    fn mark(&self, text: &str, counts: &mut Counts) -> String {
        if self.text.is_empty() {
            text.to_owned()
        } else {
            counts.marked += 1;
            format!("{text}{CHECK_MARK}{}]", self.text)
        }
    }
}

fn has_entry(entries: &[Entry], id: &str) -> bool {
    entries.iter().any(|entry| entry.id == id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Citation {
    kind: u8,
    first: usize,
    last: usize,
}

fn citations(text: &str) -> Vec<Citation> {
    const TURN_WORD: &[u8] = b"turn ";
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let starts_word = at == 0 || !is_word_byte(bytes[at - 1]);
        if starts_word
            && bytes[at..]
                .get(..TURN_WORD.len())
                .is_some_and(|word| word.eq_ignore_ascii_case(TURN_WORD))
        {
            let mut end = at + TURN_WORD.len();
            if let Some(number) = read_number(bytes, &mut end)
                && (end >= bytes.len() || !is_word_byte(bytes[end]))
            {
                found.push(Citation {
                    kind: b'M',
                    first: number,
                    last: number,
                });
                at = end;
                continue;
            }
        }
        let kind = bytes[at];
        if !matches!(kind, b'M' | b'T') || (at > 0 && is_word_byte(bytes[at - 1])) {
            at += 1;
            continue;
        }
        let mut end = at + 1;
        let Some(first) = read_number(bytes, &mut end) else {
            at += 1;
            continue;
        };
        if end < bytes.len() && is_word_byte(bytes[end]) {
            at += 1;
            continue;
        }
        let mut last = first;
        let dashes: [&[u8]; 3] = ["\u{2013}".as_bytes(), b"-", b" to "];
        if let Some(dash) = dashes.iter().find(|dash| bytes[end..].starts_with(dash)) {
            let mut range_end = end + dash.len();
            if bytes.get(range_end) == Some(&kind) {
                range_end += 1;
            }
            if let Some(upper) = read_number(bytes, &mut range_end)
                && upper > first
                && (range_end >= bytes.len() || !is_word_byte(bytes[range_end]))
            {
                last = upper;
                end = range_end;
            }
        }
        found.push(Citation { kind, first, last });
        at = end;
    }
    found
}

fn read_number(bytes: &[u8], at: &mut usize) -> Option<usize> {
    let start = *at;
    let digits = bytes[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let value = std::str::from_utf8(&bytes[start..start + digits])
        .ok()?
        .parse()
        .ok()?;
    *at = start + digits;
    Some(value)
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn check_citations(
    problems: &mut Problems,
    text: &str,
    sources: &Sources<'_>,
    counts: &mut Counts,
) {
    for citation in citations(text) {
        let count = if citation.kind == b'M' {
            sources.turn_count
        } else {
            sources.tool_count
        };
        if citation.first == 0 || citation.last > count {
            counts.missing_ids += 1;
            let shown = if citation.first == 0 {
                0
            } else {
                citation.last
            };
            problems.add(&format!(
                "{}{shown} does not exist",
                char::from(citation.kind)
            ));
        }
    }
}

fn names_this_compaction(cited: &[Citation], sources: &Sources<'_>) -> bool {
    cited.iter().any(|citation| {
        let records = if citation.kind == b'M' {
            sources.turns
        } else {
            sources.tools
        };
        find_record(records, citation.first).is_some()
            || find_record(records, citation.last).is_some()
    })
}

fn find_record(records: &[Record], wanted: usize) -> Option<&Record> {
    records
        .binary_search_by_key(&wanted, |record| record.number)
        .ok()
        .map(|index| &records[index])
}

fn check_values(problems: &mut Problems, text: &str, all: &[&str], counts: &mut Counts) {
    let mut missing = String::new();
    for value in values(text) {
        if found_in(value, all) {
            continue;
        }
        let code = text.contains(&format!("`{value}`"));
        if code && names_found(value, all) {
            continue;
        }
        counts.unfound_values += 1;
        if !missing.is_empty() {
            missing.push_str(", ");
        }
        missing.push_str(value);
    }
    if !missing.is_empty() {
        problems.add(&format!("not in the saved turns or tool calls: {missing}"));
    }
}

fn values(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    spans(&mut found, text, "`", "`", &[' ']);
    spans(&mut found, text, "\"", "\"", &CLOSING_PUNCTUATION);
    spans(
        &mut found,
        text,
        "\u{201c}",
        "\u{201d}",
        &CLOSING_PUNCTUATION,
    );
    for raw in text
        .split([' ', '\t', '\r', '\n'])
        .filter(|word| !word.is_empty())
    {
        if found.len() >= MAX_VALUES {
            break;
        }
        let word = trimmed_word(raw);
        if word.len() < 3 || !exact_looking(word) || found.contains(&word) {
            continue;
        }
        found.push(word);
    }
    found
}

fn spans<'a>(found: &mut Vec<&'a str>, text: &'a str, open: &str, close: &str, end_trim: &[char]) {
    let mut at = 0;
    while found.len() < MAX_VALUES {
        let Some(start) = text[at..].find(open).map(|offset| at + offset + open.len()) else {
            return;
        };
        let Some(stop) = text[start..].find(close).map(|offset| start + offset) else {
            return;
        };
        at = stop + close.len();
        let inner = text[start..stop]
            .trim_start_matches(' ')
            .trim_end_matches(end_trim);
        if (2..=200).contains(&inner.len()) && !found.contains(&inner) {
            found.push(inner);
        }
    }
}

fn trimmed_word(word: &str) -> &str {
    const MARKS: [&str; 5] = ["\u{201c}", "\u{201d}", "\u{2018}", "\u{2019}", "\u{2026}"];
    let mut rest = word;
    loop {
        let before = rest.len();
        rest = rest.trim_matches([
            '.', ',', ';', ':', '!', '?', '(', ')', '[', ']', '{', '}', '<', '>', '"', '\'', '`',
            '*',
        ]);
        for mark in MARKS {
            rest = rest.strip_prefix(mark).unwrap_or(rest);
            rest = rest.strip_suffix(mark).unwrap_or(rest);
        }
        if rest.len() == before {
            return rest;
        }
    }
}

fn exact_looking(word: &str) -> bool {
    if is_id_list(word) {
        return false;
    }
    let bytes = word.as_bytes();
    let mut run = 0;
    let mut longest_run = 0;
    let mut dotted_digits = false;
    for (index, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_digit() {
            run += 1;
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
        if *byte == b'.'
            && index > 0
            && index + 1 < bytes.len()
            && bytes[index - 1].is_ascii_digit()
            && bytes[index + 1].is_ascii_digit()
        {
            dotted_digits = true;
        }
    }
    if longest_run >= 3 || dotted_digits {
        return true;
    }
    if word.starts_with(['/', '~']) || word.starts_with("./") || word.contains("://") {
        return true;
    }
    let name = file_name(word);
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    let extension = &name[dot + 1..];
    dot != 0
        && (2..=6).contains(&extension.len())
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && name
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn file_name(path: &str) -> &str {
    let name = path.rfind('/').map_or(path, |slash| &path[slash + 1..]);
    name.find(':').map_or(name, |colon| &name[..colon])
}

fn is_id_list(word: &str) -> bool {
    let mut parts = word
        .split([',', '/'])
        .filter(|part| !part.is_empty())
        .peekable();
    parts.peek().is_some() && parts.all(is_id)
}

fn is_id(word: &str) -> bool {
    let bytes = word.as_bytes();
    if bytes.len() < 2 || !b"MTRFDSO".contains(&bytes[0]) || !bytes[1].is_ascii_digit() {
        return false;
    }
    let digits = 1 + bytes[1..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == bytes.len() {
        return true;
    }
    let rest = &word[digits..];
    let Some(mut upper) = rest
        .strip_prefix('\u{2013}')
        .or_else(|| rest.strip_prefix('-'))
    else {
        return false;
    };
    if upper.as_bytes().first() == Some(&bytes[0]) {
        upper = &upper[1..];
    }
    !upper.is_empty() && upper.bytes().all(|byte| byte.is_ascii_digit())
}

fn found_in(value: &str, texts: &[&str]) -> bool {
    if any_has(texts, value) {
        return true;
    }
    let path = !value.contains(' ') && (value.contains('/') || file_name(value).contains('.'));
    if path {
        let tail = path_tail(value);
        if tail.len() >= 4 && tail.len() < value.len() && any_has(texts, tail) {
            return true;
        }
    }
    numeric_part(value)
        .is_some_and(|numeric| numeric.len() >= 3 && numeric != value && any_has(texts, &numeric))
}

fn path_tail(path: &str) -> &str {
    let name_start = path.rfind('/').map_or(0, |slash| slash + 1);
    let end = path[name_start..]
        .find(':')
        .map_or(path.len(), |colon| name_start + colon);
    let trimmed = &path[..end];
    let Some(last) = trimmed.rfind('/') else {
        return trimmed;
    };
    let Some(before) = trimmed[..last].rfind('/') else {
        return trimmed;
    };
    &trimmed[before + 1..]
}

fn names_found(code: &str, texts: &[&str]) -> bool {
    let mut names = 0;
    for name in code
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|name| name.len() >= 3)
    {
        if !any_has(texts, name) {
            return false;
        }
        names += 1;
    }
    names > 0
}

fn any_has(texts: &[&str], value: &str) -> bool {
    texts.iter().any(|text| contains_ignore_case(text, value))
}

fn numeric_part(value: &str) -> Option<String> {
    let start = value.find(|character: char| character.is_ascii_digit())?;
    let mut digits = String::new();
    for character in value[start..].chars() {
        if character.is_ascii_digit() || character == '.' {
            digits.push(character);
        } else if character != ',' {
            break;
        }
    }
    let trimmed = digits.trim_end_matches('.').len();
    digits.truncate(trimmed);
    Some(digits)
}

fn check_failed_call(
    problems: &mut Problems,
    text: &str,
    number: usize,
    tools: &[Record],
    counts: &mut Counts,
) {
    if find_record(tools, number).is_some_and(|record| record.failed && calls_success(text)) {
        counts.failed_as_success += 1;
        problems.add(&format!("T{number} failed"));
    }
}

fn only_tool(cited: &[Citation]) -> Option<usize> {
    let mut found = None;
    for citation in cited.iter().filter(|citation| citation.kind == b'T') {
        if found.is_some() || citation.last != citation.first {
            return None;
        }
        found = Some(citation.first);
    }
    found
}

fn calls_success(note: &str) -> bool {
    let mut saw_success = false;
    for word in note
        .split([
            ' ', '\t', '\r', '\n', '.', ',', ';', ':', '!', '?', '(', ')', '[', ']', '"', '\'',
            '`', '-',
        ])
        .filter(|word| !word.is_empty())
    {
        if FAILURE_WORDS
            .iter()
            .any(|failure| word.eq_ignore_ascii_case(failure))
        {
            return false;
        }
        saw_success |= SUCCESS_WORDS
            .iter()
            .any(|success| word.eq_ignore_ascii_case(success));
    }
    saw_success
}

fn check_quote(problems: &mut Problems, rule: &str, users: &[String], counts: &mut Counts) {
    let mut at = 0;
    let mut quotes = 0;
    while let Some((inner, end)) = quote(rule, at) {
        at = end;
        let phrase = normalized(inner.trim_end_matches(CLOSING_PUNCTUATION));
        if phrase.is_empty() {
            continue;
        }
        quotes += 1;
        if !users.iter().any(|user| user.contains(&phrase)) {
            counts.unquoted += 1;
            problems.add("not the user's exact words");
            return;
        }
    }
    if quotes == 0 {
        counts.unquoted += 1;
        problems.add("no quote of the user's words");
    }
}

fn quote(text: &str, from: usize) -> Option<(&str, usize)> {
    let mut best: Option<(usize, &str, usize)> = None;
    for (open, close) in [("\"", "\""), ("\u{201c}", "\u{201d}")] {
        let Some(start) = text[from..].find(open).map(|offset| from + offset) else {
            continue;
        };
        if best.is_some_and(|(best_start, ..)| start >= best_start) {
            continue;
        }
        let inner = start + open.len();
        let Some(stop) = text[inner..].find(close).map(|offset| inner + offset) else {
            continue;
        };
        best = Some((start, &text[inner..stop], stop + close.len()));
    }
    best.map(|(_, inner, end)| (inner, end))
}

fn normalized(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if u8::try_from(character).is_ok_and(is_posix_space) {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(character.to_ascii_lowercase());
        }
    }
    let trimmed = out.trim_end_matches(' ').len();
    out.truncate(trimmed);
    out
}

#[cfg(test)]
mod tests;
