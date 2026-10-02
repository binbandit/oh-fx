mod saved;

use std::fmt::Write;

pub(crate) const ENTRY_KINDS: [u8; 5] = *b"RFDSO";
pub(crate) const CHECK_MARK: &str = " [check: ";
pub(crate) const REPLACED_MARK: &str = " (replaced by ";

pub(crate) use saved::{encode_checkpoint, restore_checkpoint};

pub(crate) type Highest = [usize; ENTRY_KINDS.len()];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tool {
    pub(crate) number: usize,
    pub(crate) line: String,
    pub(crate) why: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Turn {
    pub(crate) number: usize,
    pub(crate) user: String,
    pub(crate) work: String,
    pub(crate) final_reply: String,
    pub(crate) first_tool: usize,
    pub(crate) last_tool: usize,
    pub(crate) tools: Vec<Tool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OpenTurn {
    pub(crate) work: String,
    pub(crate) text: String,
    pub(crate) first_tool: usize,
    pub(crate) last_tool: usize,
    pub(crate) tools: Vec<Tool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsedKind {
    Skill,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Used {
    pub(crate) kind: UsedKind,
    pub(crate) name: String,
    pub(crate) calls: usize,
    pub(crate) first_tool: usize,
    pub(crate) last_tool: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Payload {
    pub(crate) entries: Vec<Entry>,
    pub(crate) used: Vec<Used>,
    pub(crate) turns: Vec<Turn>,
    pub(crate) open: Option<OpenTurn>,
    pub(crate) turn_count: usize,
    pub(crate) tool_count: usize,
}

pub(crate) fn replaced_ids(text: &str) -> Vec<&str> {
    const REPLACES: &str = "replaces ";
    let mut ids = Vec::new();
    let mut at = 0;
    while let Some(found) = find_ignore_case(text, at, REPLACES) {
        at = found + REPLACES.len();
        for raw in text[at..]
            .split([' ', ',', ';'])
            .filter(|word| !word.is_empty())
        {
            let word = raw.trim_end_matches(['.', ')', ']']);
            if word == "and" {
                continue;
            }
            if !is_entry_id(word) {
                break;
            }
            ids.push(word);
        }
    }
    ids
}

fn find_ignore_case(text: &str, from: usize, needle: &str) -> Option<usize> {
    let haystack = text.as_bytes().get(from..)?;
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
        .map(|offset| from + offset)
}

pub(crate) fn is_entry_id(word: &str) -> bool {
    let bytes = word.as_bytes();
    bytes.len() >= 2 && ENTRY_KINDS.contains(&bytes[0]) && bytes[1..].iter().all(u8::is_ascii_digit)
}

fn entry_number(id: &str) -> Option<(usize, usize)> {
    if !is_entry_id(id) {
        return None;
    }
    let kind = ENTRY_KINDS
        .iter()
        .position(|kind| *kind == id.as_bytes()[0])?;
    Some((kind, id[1..].parse().ok()?))
}

pub(crate) fn highest_ids(entries: &[Entry]) -> Highest {
    let mut highest = [0; ENTRY_KINDS.len()];
    for (kind, number) in entries.iter().filter_map(|entry| entry_number(&entry.id)) {
        highest[kind] = highest[kind].max(number);
    }
    highest
}

pub(crate) fn was_used(id: &str, highest: &Highest) -> bool {
    entry_number(id).is_some_and(|(kind, number)| number > 0 && number <= highest[kind])
}

pub(crate) fn render(payload: &Payload) -> String {
    let mut text = String::from(
        "<compacted_conversation>\nThis is the earlier part of this conversation, compacted. The user's messages and the assistant's final replies shown here are exact. What the assistant did in between is summarized, and each tool call has a line saying what it was and what it showed.\n\n",
    );
    for turn in &payload.turns {
        render_turn(&mut text, turn);
    }
    if let Some(open) = &payload.open {
        text.push_str("Turn in progress, whose first user message follows this:\n");
        if !open.work.is_empty() {
            let _ = write!(text, "Assistant, in between so far:\n{}\n\n", open.work);
        }
        append_tools(&mut text, "Its tools so far", &open.tools);
    }
    append_entries(&mut text, &payload.entries);
    append_used(&mut text, &payload.used);
    if has_check_mark(payload) {
        text.push_str("A note or entry marked [check: ...] says something code could not confirm in the saved turns and tool calls; confirm it there before relying on it.\n");
    }
    text.push_str("</compacted_conversation>\n");
    text
}

fn render_turn(text: &mut String, turn: &Turn) {
    let number = turn.number;
    let _ = write!(text, "Turn {number}\nUser {number}:\n{}\n\n", turn.user);
    if !turn.work.is_empty() {
        let _ = write!(text, "Assistant {number}, in between:\n{}\n\n", turn.work);
    }
    append_tools(text, "Tools", &turn.tools);
    if !turn.final_reply.is_empty() {
        let _ = write!(
            text,
            "Assistant {number}, final reply:\n{}\n\n",
            turn.final_reply
        );
    }
}

fn append_tools(text: &mut String, heading: &str, tools: &[Tool]) {
    if tools.is_empty() {
        return;
    }
    let _ = writeln!(text, "{heading}:");
    for tool in tools {
        let _ = write!(text, "  T{} {}", tool.number, tool.line);
        if !tool.why.is_empty() {
            let _ = write!(text, ": {}", tool.why);
        }
        text.push('\n');
    }
    text.push('\n');
}

fn append_entries(text: &mut String, entries: &[Entry]) {
    let mut replaced_by: Vec<Option<&str>> = vec![None; entries.len()];
    for entry in entries {
        for id in replaced_ids(&entry.text) {
            for (older, slot) in entries.iter().zip(&mut replaced_by) {
                if older.id == id {
                    *slot = Some(&entry.id);
                }
            }
        }
    }
    let sections: [(&str, &[u8]); 4] = [
        ("Rules of the session:", b"R"),
        ("Facts of the session:", b"F"),
        ("Decisions:", b"D"),
        ("Status and open:", b"SO"),
    ];
    for (heading, kinds) in sections {
        let mut written = 0;
        for (entry, replacer) in entries.iter().zip(&replaced_by) {
            if !entry
                .id
                .as_bytes()
                .first()
                .is_some_and(|kind| kinds.contains(kind))
            {
                continue;
            }
            if written == 0 {
                let _ = writeln!(text, "{heading}");
            }
            text.push_str(&entry.text);
            if let Some(id) = replacer {
                let _ = write!(text, "{REPLACED_MARK}{id})");
            }
            text.push('\n');
            written += 1;
        }
        if written > 0 {
            text.push('\n');
        }
    }
}

fn append_used(text: &mut String, used: &[Used]) {
    if used.is_empty() {
        return;
    }
    text.push_str("Skills and MCP tools used:\n");
    for entry in used {
        let kind = match entry.kind {
            UsedKind::Skill => "skill",
            UsedKind::Mcp => "MCP tool",
        };
        let _ = write!(text, "- {kind} {}: ", entry.name);
        if entry.calls == 1 {
            let _ = writeln!(text, "1 call, T{}", entry.first_tool);
        } else {
            let _ = writeln!(
                text,
                "{} calls, first T{}, last T{}",
                entry.calls, entry.first_tool, entry.last_tool
            );
        }
    }
    text.push('\n');
}

fn has_check_mark(payload: &Payload) -> bool {
    let marked = |text: &str| text.contains(CHECK_MARK);
    let tools_marked = |tools: &[Tool]| tools.iter().any(|tool| marked(&tool.why));
    payload.entries.iter().any(|entry| marked(&entry.text))
        || payload
            .turns
            .iter()
            .any(|turn| marked(&turn.work) || tools_marked(&turn.tools))
        || payload
            .open
            .as_ref()
            .is_some_and(|open| marked(&open.work) || tools_marked(&open.tools))
}

pub(crate) fn shape_problem(earlier: &Payload, payload: &Payload) -> Option<&'static str> {
    if payload.turns.len() < earlier.turns.len() {
        return Some("an earlier turn is missing");
    }
    if earlier.turns != payload.turns[..earlier.turns.len()] {
        return Some("an earlier turn changed");
    }
    if payload.entries.len() < earlier.entries.len() {
        return Some("an earlier entry is missing");
    }
    if earlier.entries != payload.entries[..earlier.entries.len()] {
        return Some("an earlier entry changed");
    }
    let mut next_turn = earlier.turn_count + 1;
    let mut previous_tool = 0;
    let mut new_tools = 0;
    for turn in &payload.turns[earlier.turns.len()..] {
        if turn.number != next_turn {
            return Some("the new turns are not numbered in order");
        }
        next_turn += 1;
        if let Some(problem) = tools_problem(
            &turn.tools,
            turn.first_tool,
            turn.last_tool,
            payload.tool_count,
            &mut previous_tool,
        ) {
            return Some(problem);
        }
        new_tools += lines_above(&turn.tools, earlier.tool_count);
    }
    if next_turn - 1 != payload.turn_count {
        return Some("the turn count does not match the turns");
    }
    if let Some(open) = &payload.open {
        if let Some(problem) = tools_problem(
            &open.tools,
            open.first_tool,
            open.last_tool,
            payload.tool_count,
            &mut previous_tool,
        ) {
            return Some(problem);
        }
        new_tools += lines_above(&open.tools, earlier.tool_count);
    }
    if earlier.tool_count + new_tools != payload.tool_count {
        return Some("a new tool call has no line");
    }
    for (index, entry) in payload.entries.iter().enumerate() {
        if !is_entry_id(&entry.id) {
            return Some("an entry has a malformed ID");
        }
        if !entry.text.starts_with(&entry.id) {
            return Some("an entry does not start with its ID");
        }
        if payload.entries[..index]
            .iter()
            .any(|other| other.id == entry.id)
        {
            return Some("two entries share an ID");
        }
    }
    None
}

fn tools_problem(
    tools: &[Tool],
    first: usize,
    last: usize,
    count: usize,
    previous: &mut usize,
) -> Option<&'static str> {
    if first > last || last > count {
        return Some("a turn's tool calls are out of range");
    }
    for tool in tools {
        if tool.number < first || tool.number > last {
            return Some("a tool line is outside its turn");
        }
        if tool.number <= *previous {
            return Some("the tool lines are out of order");
        }
        if tool.line.is_empty() {
            return Some("a tool line is empty");
        }
        *previous = tool.number;
    }
    None
}

fn lines_above(tools: &[Tool], number: usize) -> usize {
    tools.iter().filter(|tool| tool.number > number).count()
}

#[cfg(test)]
mod tests;
