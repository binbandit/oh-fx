use std::borrow::Cow;
use std::fmt::Write;

use serde_json::Value;

use super::checkpoint::{
    ENTRY_KINDS, Entry, Highest, REPLACED_MARK, Used, UsedKind, highest_ids, is_entry_id,
};
use super::trace::Tracer;

const TURN_LABEL: &str = "Turn";
const IN_BETWEEN_LABEL: &str = "In between:";
const FROM_OH_FX: &str = " This request comes from oh-fx, not from the user, so leave it and the writing of these notes out of every note and entry.";
const MAX_CANDIDATE_BYTES: usize = 12 * 1024;
const MAX_CANDIDATE_LINE_BYTES: usize = 400;
const MAX_WORK_BYTES: usize = 1200;
const MAX_TOOL_NOTE_BYTES: usize = 300;
const MAX_ID_GAP: usize = 1000;
const MAX_UNREAD_BYTES: usize = 8 * 1024;
const MAX_EARLIER_BYTES: usize = 2400;
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Heading {
    pub(crate) number: usize,
    pub(crate) first_tool: usize,
    pub(crate) last_tool: usize,
    pub(crate) begins: String,
    pub(crate) tools: Vec<String>,
}

pub(crate) struct Asked<'a> {
    pub(crate) turns: &'a [Heading],
    pub(crate) open: Option<&'a Heading>,
    pub(crate) highest: Highest,
    pub(crate) after_conversation: bool,
}

pub(crate) fn write_request(text: &mut String, asked: &Asked<'_>) {
    if asked.after_conversation {
        text.push_str("Write the compaction notes for the turns of the conversation above that are listed below; the turns after them stay in the conversation as they are. Answer with text only and call no tools.");
        text.push_str(FROM_OH_FX);
    } else {
        text.push_str("Write the compaction notes for the new turns above.");
    }
    text.push_str(" The user's messages and the assistant's final replies stay in the conversation word for word, so do not repeat them.");
    text.push_str(" The tool calls will not be available later, so keep in your notes the details from them that the work still needs.");
    if asked.turns.is_empty() && asked.open.is_none() {
        text.push_str("\n\n");
    } else {
        text.push_str("\n\nWrite these headings, in this order and without skipping any, each followed by its notes:\n\n");
        write_headings(text, asked.turns, asked.open);
        if asked.after_conversation {
            text.push_str("\nUnder each heading above are the turn's tool calls in order, with their IDs, so you can find them in the conversation; do not copy those lines.\n");
        }
        let _ = write!(
            text,
            "\nUnder each heading:\n{IN_BETWEEN_LABEL} one to three sentences on what the assistant did before its final reply: what it looked at, what it found, what it changed or decided. Leave out what the final reply already says. Write \"none\" when there was nothing.\nThen one line for every tool call of the turn, starting with its ID like `T<number>:`, at most 15 words: why it was used and what it showed that matters. Calls with one purpose may share a line that starts `T<first>\u{2013}T<last>:`.\n"
        );
        if asked.open.is_some() {
            text.push_str(if asked.after_conversation {
                "For the turn in progress, give its notes so far, through the last tool call listed under its heading; the calls after it stay in the conversation.\n"
            } else {
                "For the turn in progress, give its notes so far.\n"
            });
        }
        text.push('\n');
    }
    text.push_str("Then only the new entries of these sections, each starting with its ID and the turn or tool call it comes from, like `F<number> (T<number>):`:\n\n");
    let _ = write!(
        text,
        "Rules:\nR1, R2, ...: each new instruction, rule or preference from the user, quoted word for word in double quotes, with its turn, like `(turn <number>)`{}.\n\n",
        if asked.open.is_some() {
            ", or `(turn in progress)` for the turn still in progress"
        } else {
            ""
        }
    );
    text.push_str("Facts:\nF1, F2, ...: facts the work depends on, from these turns: names, paths, values, results, causes.\n\nDecisions:\nD1, D2, ...: each decision and why. When it changes an earlier entry, end with \"replaces\" and that entry's ID.\n\nStatus:\nS1, S2, ...: where each part of the work stands now. When it updates an earlier entry, or answers or finishes an earlier open entry, end with \"replaces\" and that entry's ID.\n\nOpen:\nO1, O2, ...: questions waiting on the user, and next steps the user asked for.\n\n");
    text.push_str("Never repeat or rewrite an entry that already exists; add a new one that replaces it. Write \"none\" under a section with nothing new.");
    write_highest_ids(text, &asked.highest);
    text.push_str(" Be exact: say what was verified, and mark anything only planned, assumed or not checked. Write only these notes.");
}

pub(crate) fn write_follow_up(
    text: &mut String,
    missing: &[Heading],
    highest: &Highest,
    after_conversation: bool,
) {
    text.push_str("Your notes on the turns above left some out. Write the notes for only these turns now, each heading followed by its notes:\n\n");
    write_headings(text, missing, None);
    if after_conversation {
        let _ = writeln!(
            text,
            "\nUnder each heading above are the turn's tool calls, so you can find them; do not copy those lines. Answer with text only and call no tools.{FROM_OH_FX}"
        );
    }
    let _ = write!(
        text,
        "\nUnder each heading, {IN_BETWEEN_LABEL} with what the assistant did before its final reply, then a line for every tool call, starting with its ID, on why it was used and what it showed.\n\nThen any new entries from those turns under the same sections, each starting with its ID and the turn or tool call it comes from."
    );
    write_highest_ids(text, highest);
    text.push_str(" Write only these notes.");
}

fn write_headings(text: &mut String, turns: &[Heading], open: Option<&Heading>) {
    for turn in turns {
        let _ = write!(text, "{TURN_LABEL} {}", turn.number);
        write_heading_rest(text, turn);
    }
    if let Some(turn) = open {
        let _ = write!(text, "{TURN_LABEL} in progress");
        write_heading_rest(text, turn);
    }
}

fn write_heading_rest(text: &mut String, turn: &Heading) {
    if turn.first_tool == 0 {
        text.push_str(" (no tool calls)");
    } else if turn.first_tool == turn.last_tool {
        let _ = write!(text, " (T{})", turn.first_tool);
    } else {
        let _ = write!(text, " (T{}\u{2013}T{})", turn.first_tool, turn.last_tool);
    }
    if !turn.begins.is_empty() {
        let _ = write!(text, ", which begins \u{201c}{}\u{201d}", turn.begins);
    }
    text.push('\n');
    for line in &turn.tools {
        let _ = writeln!(text, "  {line}");
    }
}

fn write_highest_ids(text: &mut String, highest: &Highest) {
    let mut written = 0;
    for (kind, number) in ENTRY_KINDS.iter().zip(highest) {
        if *number == 0 {
            continue;
        }
        let lead = if written == 0 {
            " The highest IDs so far: "
        } else {
            ", "
        };
        let _ = write!(text, "{lead}{}{number}", char::from(*kind));
        written += 1;
    }
    text.push_str(if written > 0 {
        ". Number new entries after them."
    } else {
        " Number each kind from 1."
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Message<'a> {
    pub(crate) turn: usize,
    pub(crate) text: &'a str,
    pub(crate) in_progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) turn: usize,
    pub(crate) text: String,
    pub(crate) in_progress: bool,
}

pub(crate) fn candidates(
    messages: &[Message<'_>],
    filed: &[Entry],
    trace: Tracer,
) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = Vec::new();
    let mut bytes = 0;
    for message in messages {
        let mut before = ["", ""];
        let mut fenced = false;
        for raw_line in message.text.split('\n') {
            let line = raw_line.trim_matches([' ', '\t', '\r']);
            if line.starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if fenced
                || line.is_empty()
                || line.len() > MAX_CANDIDATE_LINE_BYTES
                || !looks_written(line)
            {
                continue;
            }
            for sentence in (Sentences { text: line, at: 0 }) {
                let previous = before;
                before = [before[1], sentence];
                if sentence.ends_with('?') {
                    continue;
                }
                let words = lower_words(sentence);
                if !has_rule_words(&words) {
                    continue;
                }
                let text = if needs_context(&words) {
                    join_sentences(&[previous[0], previous[1], sentence])
                } else {
                    sentence.to_owned()
                };
                let seen = found.iter().any(|item| item.text == text);
                let quoted = filed
                    .iter()
                    .any(|entry| entry.id.starts_with('R') && entry.text.contains(sentence));
                if seen || quoted {
                    continue;
                }
                if bytes + text.len() > MAX_CANDIDATE_BYTES {
                    trace.log(
                        false,
                        format_args!(
                            "rule candidates over their room; the newest are left out candidates={} bytes={bytes}",
                            found.len()
                        ),
                    );
                    return found;
                }
                bytes += text.len();
                found.push(Candidate {
                    turn: message.turn,
                    text,
                    in_progress: message.in_progress,
                });
            }
        }
    }
    found
}

pub(crate) fn write_candidates(text: &mut String, found: &[Candidate]) {
    if found.is_empty() {
        return;
    }
    text.push_str("\n\nSentences from the user's messages that may set rules, found by code. File each one that still applies under Rules, quoted exactly with its turn ID; leave out complaints, reports and requests meant only for that moment:\n");
    for candidate in found {
        if candidate.in_progress {
            text.push_str("- turn in progress: ");
        } else {
            let _ = write!(text, "- turn {}: ", candidate.turn);
        }
        let _ = writeln!(text, "\"{}\"", candidate.text);
    }
}

fn looks_written(line: &str) -> bool {
    if ["\u{2503}", "\u{2502}", "|", ">"]
        .iter()
        .any(|quoted| line.starts_with(quoted))
    {
        return false;
    }
    let written = line
        .bytes()
        .filter(|byte| byte.is_ascii_alphabetic() || matches!(byte, b' ' | b'\'' | b','))
        .count();
    written * 10 >= line.len() * 7
}

struct Sentences<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Iterator for Sentences<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let bytes = self.text.as_bytes();
        while self.at < bytes.len() {
            let start = self.at;
            let mut end = start;
            while end < bytes.len() {
                let ends_sentence = matches!(bytes[end], b'.' | b'!' | b'?')
                    && (end + 1 == bytes.len() || bytes[end + 1] == b' ');
                end += 1;
                if ends_sentence {
                    break;
                }
            }
            self.at = end;
            let sentence = self.text[start..end].trim_matches(' ');
            if !sentence.is_empty() {
                return Some(sentence);
            }
        }
        None
    }
}

fn lower_words(sentence: &str) -> Vec<String> {
    let plain = sentence.replace('\u{2019}', "'").to_ascii_lowercase();
    plain
        .split([
            ' ', '\t', ',', ';', ':', '(', ')', '"', '`', '*', '_', '.', '!',
        ])
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn has_rule_words(words: &[String]) -> bool {
    const SINGLE: [&str; 12] = [
        "never", "don't", "dont", "avoid", "always", "only", "must", "mustn't", "every", "ignore",
        "skip", "stop",
    ];
    const PAIRS: [(&str, &str); 7] = [
        ("do", "not"),
        ("instead", "of"),
        ("rather", "than"),
        ("at", "most"),
        ("make", "sure"),
        ("no", "longer"),
        ("no", "more"),
    ];
    const AFTER_NOT: [&str; 7] = ["a", "an", "the", "by", "from", "to", "in"];
    for (index, word) in words.iter().enumerate() {
        let next = words.get(index + 1).map_or("", String::as_str);
        if SINGLE.contains(&word.as_str())
            || PAIRS.contains(&(word.as_str(), next))
            || (word == "not" && AFTER_NOT.contains(&next))
            || (word == "under" && next.as_bytes().first().is_some_and(u8::is_ascii_digit))
        {
            return true;
        }
    }
    false
}

fn needs_context(words: &[String]) -> bool {
    const SKIP: [&str; 13] = [
        "don't", "dont", "do", "not", "never", "only", "always", "please", "just", "so", "and",
        "but", "then",
    ];
    const PRONOUNS: [&str; 6] = ["it", "that", "this", "them", "those", "these"];
    if words.len() < 7 {
        return true;
    }
    words
        .iter()
        .find(|word| !SKIP.contains(&word.as_str()))
        .is_some_and(|word| PRONOUNS.contains(&word.as_str()))
}

fn join_sentences(parts: &[&str]) -> String {
    let mut text = String::new();
    for part in parts.iter().filter(|part| !part.is_empty()) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(part);
    }
    text
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Note {
    pub(crate) number: usize,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Known {
    pub(crate) turns: Vec<usize>,
    pub(crate) tools: Vec<usize>,
    pub(crate) open: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Written {
    pub(crate) works: Vec<Note>,
    pub(crate) tools: Vec<Note>,
    pub(crate) entries: Vec<Entry>,
    pub(crate) noted: Vec<usize>,
    pub(crate) earlier: String,
    pub(crate) repeated: usize,
    pub(crate) unknown: usize,
}

impl Written {
    pub(crate) fn work(&self, turn: usize) -> &str {
        note_for(&self.works, turn)
    }

    pub(crate) fn tool(&self, number: usize) -> &str {
        note_for(&self.tools, number)
    }
}

fn note_for(notes: &[Note], number: usize) -> &str {
    notes
        .iter()
        .find(|note| note.number == number)
        .map_or("", |note| &note.text)
}

struct Building {
    number: usize,
    text: String,
    limit: usize,
}

#[derive(Clone, Copy)]
enum Current {
    Work(usize),
    Tool(usize),
}

#[derive(Default)]
struct Reader {
    works: Vec<Building>,
    tool_notes: Vec<Building>,
    sections: String,
    earlier: String,
    current: Option<Current>,
    turn: Option<usize>,
    in_sections: bool,
    in_earlier: bool,
    unknown: usize,
    noted: Vec<usize>,
}

impl Reader {
    fn line(&mut self, raw: &str, known: &Known) {
        let line = raw.trim_matches([' ', '\t', '\r']);
        let plain = undecorated(line);
        if let Some(number) = turn_number(&plain) {
            self.enter(Some(number), false, false);
        } else if let Some(rest) = earlier_rest(&plain) {
            self.enter(None, false, true);
            if !rest.is_empty() {
                self.earlier.push_str(rest);
                self.earlier.push('\n');
            }
        } else if let Some(rest) = section_rest(&plain) {
            self.enter(None, true, false);
            self.sections.push_str(rest);
            self.sections.push('\n');
        } else if self.in_earlier {
            self.earlier.push_str(line);
            self.earlier.push('\n');
        } else if self.in_sections {
            self.sections.push_str(raw);
            self.sections.push('\n');
        } else if let Some(number) = self.turn {
            if let Some(work_text) = in_between_rest(&plain) {
                self.work(number, work_text, known);
            } else if let Some(found) = tool_note(&plain) {
                self.tool(&found, known);
            } else if is_tools_label(&plain) || line.is_empty() {
                self.current = None;
            } else {
                self.continue_note(line);
            }
        }
    }

    fn enter(&mut self, turn: Option<usize>, in_sections: bool, in_earlier: bool) {
        self.turn = turn;
        self.in_sections = in_sections;
        self.in_earlier = in_earlier;
        self.current = None;
    }

    fn work(&mut self, number: usize, text: &str, known: &Known) {
        self.current = None;
        let allowed = if number == 0 {
            known.open
        } else {
            known.turns.contains(&number)
        };
        if !allowed {
            self.unknown += 1;
            return;
        }
        if number > 0 && !self.noted.contains(&number) {
            self.noted.push(number);
        }
        self.works.push(Building {
            number,
            text: text.to_owned(),
            limit: MAX_WORK_BYTES,
        });
        self.current = Some(Current::Work(self.works.len() - 1));
    }

    fn tool(&mut self, found: &ToolNote<'_>, known: &Known) {
        self.current = None;
        if !known.tools.contains(&found.number)
            || (found.last > 0 && !known.tools.contains(&found.last))
        {
            self.unknown += 1;
            return;
        }
        if is_none(found.text) {
            return;
        }
        let mut text = String::new();
        if found.last > 0 {
            let _ = write!(text, "T{}\u{2013}T{}: ", found.number, found.last);
        }
        text.push_str(found.text);
        self.tool_notes.push(Building {
            number: found.number,
            text,
            limit: MAX_TOOL_NOTE_BYTES,
        });
        self.current = Some(Current::Tool(self.tool_notes.len() - 1));
    }

    fn continue_note(&mut self, line: &str) {
        let building = match self.current {
            Some(Current::Work(index)) => &mut self.works[index],
            Some(Current::Tool(index)) => &mut self.tool_notes[index],
            None => return,
        };
        building.text.push(' ');
        building.text.push_str(line);
    }

    fn read_nothing(&self) -> bool {
        self.works.is_empty()
            && self.tool_notes.is_empty()
            && self.sections.is_empty()
            && self.earlier.is_empty()
            && self.unknown == 0
    }
}

pub(crate) fn read(reply: &str, known: &Known, earlier: &[Entry], trace: Tracer) -> Written {
    let mut reader = Reader::default();
    for raw in reply.split('\n') {
        reader.line(raw, known);
    }
    if reader.read_nothing() {
        let newest = if known.open {
            Some(0)
        } else {
            known.turns.last().copied()
        };
        if let Some(number) = newest {
            let text = reply.trim_matches(TRIMMED);
            if number > 0 && !text.is_empty() {
                reader.noted.push(number);
            }
            reader.works.push(Building {
                number,
                text: text.to_owned(),
                limit: MAX_UNREAD_BYTES,
            });
        } else {
            reader.earlier.push_str(reply);
        }
    }
    let works = finished_notes(reader.works, trace);
    let tools = finished_notes(reader.tool_notes, trace);
    let earlier_summary = earlier_summary(&reader.earlier, trace);
    let entries = new_entries(&reader.sections, earlier);
    if entries.renumbered > 0 {
        trace.log(
            false,
            format_args!(
                "compaction entries renumbered because their IDs were taken or far above the highest count={}",
                entries.renumbered
            ),
        );
    }
    Written {
        works,
        tools,
        entries: entries.entries,
        noted: reader.noted,
        earlier: earlier_summary,
        repeated: entries.repeated,
        unknown: reader.unknown,
    }
}

fn finished_notes(building: Vec<Building>, trace: Tracer) -> Vec<Note> {
    let mut notes: Vec<Note> = Vec::new();
    for note in building {
        let text = note.text.trim_matches(' ');
        if text.is_empty() || is_none(text) || !note_for(&notes, note.number).is_empty() {
            continue;
        }
        let cut = text.floor_char_boundary(text.len().min(note.limit));
        if cut < text.len() {
            trace.log(
                false,
                format_args!(
                    "a compaction note was cut to {cut} bytes number={} bytes={}",
                    note.number,
                    text.len()
                ),
            );
        }
        notes.push(Note {
            number: note.number,
            text: text[..cut].to_owned(),
        });
    }
    notes
}

fn earlier_summary(written: &str, trace: Tracer) -> String {
    let summary = written.trim_matches(TRIMMED);
    if is_none(summary) {
        return String::new();
    }
    let cut = summary.floor_char_boundary(summary.len().min(MAX_EARLIER_BYTES));
    if cut < summary.len() {
        trace.log(
            false,
            format_args!(
                "the summary of earlier compactions was cut to {cut} bytes bytes={}",
                summary.len()
            ),
        );
    }
    summary[..cut].to_owned()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fate {
    Repeat,
    Keep,
    Renumber,
}

struct NewEntries {
    entries: Vec<Entry>,
    repeated: usize,
    renumbered: usize,
}

fn new_entries(sections: &str, earlier: &[Entry]) -> NewEntries {
    let found = items(sections);
    let mut repeated = 0;
    let mut renumbered = 0;
    let mut fates = Vec::with_capacity(found.len());
    let before = highest_ids(earlier);
    let mut next = before;
    for (index, item) in found.iter().enumerate() {
        let rest = entry_rest(item);
        let repeats = earlier
            .iter()
            .any(|entry| entry.id == item.id && entry.text[entry.id.len()..] == *rest);
        if repeats || copies_replaced(rest) {
            fates.push(Fate::Repeat);
            repeated += 1;
            continue;
        }
        let kind = entry_kind(item.id);
        let number = item.id[1..].parse::<usize>().unwrap_or(0);
        let reused = found[..index]
            .iter()
            .zip(&fates)
            .any(|(other, fate)| *fate == Fate::Keep && other.id == item.id);
        if number <= before[kind] || reused || number - before[kind] > MAX_ID_GAP {
            fates.push(Fate::Renumber);
            continue;
        }
        fates.push(Fate::Keep);
        next[kind] = next[kind].max(number);
    }
    let mut entries = Vec::new();
    for (item, fate) in found.iter().zip(fates) {
        let id = match fate {
            Fate::Repeat => continue,
            Fate::Keep => item.id.to_owned(),
            Fate::Renumber => {
                let kind = entry_kind(item.id);
                next[kind] = next[kind].saturating_add(1);
                renumbered += 1;
                format!("{}{}", char::from(ENTRY_KINDS[kind]), next[kind])
            }
        };
        let text = format!("{id}{}", entry_rest(item));
        entries.push(Entry { id, text });
    }
    NewEntries {
        entries,
        repeated,
        renumbered,
    }
}

fn entry_kind(id: &str) -> usize {
    ENTRY_KINDS
        .iter()
        .position(|kind| id.as_bytes().first() == Some(kind))
        .unwrap_or_default()
}

fn is_none(text: &str) -> bool {
    text.trim_end_matches('.').eq_ignore_ascii_case("none")
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.as_bytes().get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix.as_bytes())
        .then(|| &text[prefix.len()..])
}

fn undecorated(line: &str) -> Cow<'_, str> {
    let start = line.trim_start_matches(['#', '-', '*', '>', ' ', '\t']);
    if start.contains("**") {
        Cow::Owned(start.replace("**", ""))
    } else {
        Cow::Borrowed(start)
    }
}

fn turn_number(plain: &str) -> Option<usize> {
    let rest = plain.trim_matches(['[', ']', ':', ' ']);
    let rest = strip_prefix_ignore_case(rest, "Turn ")?.trim_start_matches(['[', ' ']);
    if let Some(after) = strip_prefix_ignore_case(rest, "in progress") {
        return ends_heading(after).then_some(0);
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || !ends_heading(&rest[digits..]) {
        return None;
    }
    rest[..digits].parse().ok().filter(|number| *number > 0)
}

fn ends_heading(rest: &str) -> bool {
    let after = rest.trim_start_matches(' ');
    after.is_empty()
        || ["(", ":", "]", "\u{2013}", "\u{2014}", "-"]
            .iter()
            .any(|mark| after.starts_with(mark))
}

fn in_between_rest(plain: &str) -> Option<&str> {
    [IN_BETWEEN_LABEL, "In-between:"]
        .iter()
        .find_map(|label| strip_prefix_ignore_case(plain, label))
        .map(|rest| rest.trim_matches(' '))
}

fn earlier_rest(plain: &str) -> Option<&str> {
    heading_rest(plain, &["Earlier summary", "Earlier"])
}

fn section_rest(plain: &str) -> Option<&str> {
    heading_rest(
        plain,
        &[
            "Rules of the session",
            "Facts of the session",
            "Status and open",
            "Rules",
            "Facts",
            "Decisions",
            "Status",
            "Open",
        ],
    )
}

fn heading_rest<'a>(plain: &'a str, headings: &[&str]) -> Option<&'a str> {
    for heading in headings {
        let Some(after) = strip_prefix_ignore_case(plain, heading) else {
            continue;
        };
        if after.is_empty() {
            return Some(after);
        }
        if let Some(rest) = after.strip_prefix(':') {
            return Some(rest.trim_matches(' '));
        }
    }
    None
}

struct ToolNote<'a> {
    number: usize,
    last: usize,
    text: &'a str,
}

fn tool_note(plain: &str) -> Option<ToolNote<'_>> {
    let mut rest = plain;
    let number = tool_number(&mut rest)?;
    let mut last = 0;
    let mut after = rest.trim_start_matches(' ');
    if let Some(dash) = ["\u{2013}", "\u{2014}", "-", "to "]
        .iter()
        .find(|dash| after.starts_with(*dash))
    {
        let mut end = after[dash.len()..].trim_start_matches(' ');
        if let Some(range_end) = tool_number(&mut end)
            && range_end > number
        {
            last = range_end;
            after = end.trim_start_matches(' ');
        }
    }
    after = skip_references(after)?;
    [":", "-", "\u{2014}", "\u{2013}"]
        .iter()
        .find_map(|separator| after.strip_prefix(separator))
        .map(|text| ToolNote {
            number,
            last,
            text: text.trim_matches(' '),
        })
}

fn skip_references(mut text: &str) -> Option<&str> {
    while let Some(open) = text
        .as_bytes()
        .first()
        .filter(|byte| matches!(byte, b'(' | b'['))
    {
        let close = if *open == b'(' { ')' } else { ']' };
        let at = text.find(close)?;
        text = text[at + 1..].trim_start_matches(' ');
    }
    Some(text)
}

fn tool_number(text: &mut &str) -> Option<usize> {
    let rest = text.strip_prefix('T')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let number = rest[..digits].parse().ok()?;
    *text = &rest[digits..];
    Some(number)
}

fn is_tools_label(plain: &str) -> bool {
    strip_prefix_ignore_case(plain, "T:").is_some()
        || strip_prefix_ignore_case(plain, "Tools:").is_some()
}

fn copies_replaced(rest: &str) -> bool {
    let line = rest.trim_end_matches([' ', '.']);
    let Some(at) = line.rfind(REPLACED_MARK) else {
        return false;
    };
    let tail = &line[at + REPLACED_MARK.len()..];
    tail.len() > 1 && tail.strip_suffix(')').is_some_and(is_entry_id)
}

fn entry_rest<'a>(item: &Item<'a>) -> &'a str {
    let at = item.text.find(item.id).unwrap_or_default();
    let rest = &item.text[at + item.id.len()..];
    rest.strip_prefix("**").unwrap_or(rest)
}

struct Item<'a> {
    id: &'a str,
    text: &'a str,
}

fn items(text: &str) -> Vec<Item<'_>> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let line_end = text[at..]
            .find('\n')
            .map_or(text.len(), |offset| at + offset);
        let Some(id) = item_id(&text[at..line_end]) else {
            at = line_end + 1;
            continue;
        };
        let mut end = line_end;
        while end < text.len() {
            let next_end = text[end + 1..]
                .find('\n')
                .map_or(text.len(), |offset| end + 1 + offset);
            let next = &text[end + 1..next_end];
            let continues = next.starts_with([' ', '\t'])
                && !next.trim_matches([' ', '\t']).is_empty()
                && item_id(next).is_none();
            if !continues {
                break;
            }
            end = next_end;
        }
        found.push(Item {
            id,
            text: &text[at..end],
        });
        at = end + 1;
    }
    found
}

fn item_id(line: &str) -> Option<&str> {
    let mut rest = line.trim_start_matches([' ', '\t']);
    loop {
        if let Some(after) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
            rest = after;
        } else if rest.len() >= 3 && rest.as_bytes()[0] == b'[' && rest.as_bytes()[2] == b']' {
            rest = rest[3..].trim_start_matches(' ');
        } else if let Some(after) = rest.strip_prefix("**") {
            rest = after;
        } else {
            break;
        }
    }
    if rest.len() < 3 || !ENTRY_KINDS.contains(&rest.as_bytes()[0]) {
        return None;
    }
    let digits = 1 + rest[1..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 1 {
        return None;
    }
    let mut after = &rest[digits..];
    after = after.strip_prefix("**").unwrap_or(after);
    after = skip_references(after.trim_start_matches(' '))?;
    after
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(byte, b':' | b'.' | b')'))
        .then(|| &rest[..digits])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Call<'a> {
    pub(crate) number: usize,
    pub(crate) name: &'a str,
    pub(crate) arguments: &'a str,
}

pub(crate) fn add_used(earlier: &[Used], calls: &[Call<'_>]) -> Vec<Used> {
    let mut used = earlier.to_vec();
    for call in calls {
        let Some((kind, name)) = used_by(call) else {
            continue;
        };
        if let Some(existing) = used
            .iter_mut()
            .find(|existing| existing.kind == kind && existing.name == name)
        {
            existing.calls += 1;
            existing.last_tool = call.number;
        } else {
            used.push(Used {
                kind,
                name,
                calls: 1,
                first_tool: call.number,
                last_tool: call.number,
            });
        }
    }
    used
}

fn used_by(call: &Call<'_>) -> Option<(UsedKind, String)> {
    match call.name {
        "skill" => {
            let arguments = object_arguments(call.arguments)?;
            let location = string_field(&arguments, "location")?;
            let name = match string_field(&arguments, "resource") {
                Some(resource) if !resource.is_empty() => format!("{location} {resource}"),
                _ => location.to_owned(),
            };
            Some((UsedKind::Skill, name))
        }
        "mcp_features" => {
            let arguments = object_arguments(call.arguments);
            let field = |name| {
                arguments
                    .as_ref()
                    .and_then(|arguments| string_field(arguments, name))
                    .unwrap_or_default()
            };
            Some((
                UsedKind::Mcp,
                format!("mcp_features {} {}", field("server"), field("action")),
            ))
        }
        name if name.starts_with("mcp_") && name != "mcp_select_tool" => {
            Some((UsedKind::Mcp, name.to_owned()))
        }
        _ => None,
    }
}

fn object_arguments(arguments: &str) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_str(arguments).ok()? {
        Value::Object(fields) => Some(fields),
        _ => None,
    }
}

fn string_field<'a>(arguments: &'a serde_json::Map<String, Value>, field: &str) -> Option<&'a str> {
    arguments.get(field)?.as_str()
}

#[cfg(test)]
mod tests;
