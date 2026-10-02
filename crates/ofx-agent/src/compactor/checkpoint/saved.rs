use serde_json::{Map, Value, json};

use super::{
    ENTRY_KINDS, Entry, OpenTurn, Payload, Tool, Turn, Used, UsedKind, highest_ids, render,
};

const MARKER: &str = "fx-compactor-v1\n";
const CONTINUATION_PREAMBLE: &str = "This session is being continued from earlier compacted context. The summary below covers the earlier portion of the conversation.\n\n";
const RECENT_MESSAGES_NOTE: &str = "Recent conversation turns are preserved verbatim.";
const DIRECT_RESUME_INSTRUCTION: &str = "Continue the conversation from where it left off without asking the user to repeat context. Resume directly.";

type Object = Map<String, Value>;

pub(crate) fn encode_checkpoint(payload: &Payload) -> String {
    let entries: Vec<Value> = payload
        .entries
        .iter()
        .map(|entry| json!({"id": entry.id, "text": entry.text}))
        .collect();
    let used: Vec<Value> = payload
        .used
        .iter()
        .map(|used| {
            json!({
                "kind": match used.kind {
                    UsedKind::Skill => "skill",
                    UsedKind::Mcp => "mcp",
                },
                "name": used.name,
                "calls": used.calls,
                "first_tool": used.first_tool,
                "last_tool": used.last_tool,
            })
        })
        .collect();
    let turns: Vec<Value> = payload
        .turns
        .iter()
        .map(|turn| {
            json!({
                "number": turn.number,
                "users": [turn.user],
                "work": turn.work,
                "final": turn.final_reply,
                "first_tool": turn.first_tool,
                "last_tool": turn.last_tool,
                "tools": tools_json(&turn.tools),
            })
        })
        .collect();
    let open = payload.open.as_ref().map(|open| {
        json!({
            "users": [],
            "work": open.work,
            "text": open.text,
            "first_tool": open.first_tool,
            "last_tool": open.last_tool,
            "tools": tools_json(&open.tools),
        })
    });
    let saved = json!({
        "entries": entries,
        "used": used,
        "earlier": "",
        "turns": turns,
        "open": open,
        "turn_count": payload.turn_count,
        "tool_count": payload.tool_count,
        "ledger_count": 0,
        "highest": highest_ids(&payload.entries),
        "saved": false,
    });
    format!("{MARKER}{saved}")
}

pub(crate) fn restore_checkpoint(summary: &str) -> (String, Option<Payload>) {
    let Some(json) = summary.strip_prefix(MARKER) else {
        return (
            format!(
                "{CONTINUATION_PREAMBLE}{summary}\n\n{RECENT_MESSAGES_NOTE}\n{DIRECT_RESUME_INSTRUCTION}"
            ),
            None,
        );
    };
    let payload = serde_json::from_str::<Value>(json)
        .ok()
        .as_ref()
        .and_then(Value::as_object)
        .and_then(payload_from);
    match payload {
        Some(payload) => (render(&payload), Some(payload)),
        None => (json.to_owned(), None),
    }
}

fn tools_json(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| json!({"number": tool.number, "line": tool.line, "why": tool.why}))
        .collect()
}

fn payload_from(object: &Object) -> Option<Payload> {
    let representable = text(object, "earlier")?.is_empty()
        && count(object, "ledger_count")? == 0
        && !flag(object, "saved", true)?
        && highest_is_valid(object);
    if !representable {
        return None;
    }
    let open = match object.get("open") {
        None | Some(Value::Null) => None,
        Some(open) => Some(open_from(open.as_object()?)?),
    };
    Some(Payload {
        entries: objects(object, "entries")?
            .map(|entry| {
                Some(Entry {
                    id: required_text(entry?, "id")?,
                    text: required_text(entry?, "text")?,
                })
            })
            .collect::<Option<_>>()?,
        used: objects(object, "used")?
            .map(|used| used_from(used?))
            .collect::<Option<_>>()?,
        turns: objects(object, "turns")?
            .map(|turn| turn_from(turn?))
            .collect::<Option<_>>()?,
        open,
        turn_count: count(object, "turn_count")?,
        tool_count: count(object, "tool_count")?,
    })
}

fn used_from(object: &Object) -> Option<Used> {
    let kind = match object.get("kind")?.as_str()? {
        "skill" => UsedKind::Skill,
        "mcp" => UsedKind::Mcp,
        _ => return None,
    };
    Some(Used {
        kind,
        name: required_text(object, "name")?,
        calls: match object.get("calls") {
            None => 1,
            Some(calls) => usize::try_from(calls.as_u64()?).ok()?,
        },
        first_tool: count(object, "first_tool")?,
        last_tool: count(object, "last_tool")?,
    })
}

fn turn_from(object: &Object) -> Option<Turn> {
    let mut users = list(object, "users")?.iter();
    let (Some(user), None) = (users.next(), users.next()) else {
        return None;
    };
    let number = count(object, "number")?;
    (number > 0).then_some(())?;
    Some(Turn {
        number,
        user: user.as_str()?.to_owned(),
        work: text(object, "work")?.to_owned(),
        final_reply: text(object, "final")?.to_owned(),
        first_tool: count(object, "first_tool")?,
        last_tool: count(object, "last_tool")?,
        tools: tools_from(object)?,
    })
}

fn open_from(object: &Object) -> Option<OpenTurn> {
    if !list(object, "users")?.is_empty() {
        return None;
    }
    Some(OpenTurn {
        work: text(object, "work")?.to_owned(),
        text: text(object, "text")?.to_owned(),
        first_tool: count(object, "first_tool")?,
        last_tool: count(object, "last_tool")?,
        tools: tools_from(object)?,
    })
}

fn tools_from(object: &Object) -> Option<Vec<Tool>> {
    objects(object, "tools")?
        .map(|tool| {
            let tool = tool?;
            Some(Tool {
                number: usize::try_from(tool.get("number")?.as_u64()?).ok()?,
                line: required_text(tool, "line")?,
                why: text(tool, "why")?.to_owned(),
            })
        })
        .collect()
}

fn highest_is_valid(object: &Object) -> bool {
    match object.get("highest") {
        None => true,
        Some(highest) => highest.as_array().is_some_and(|numbers| {
            numbers.len() == ENTRY_KINDS.len() && numbers.iter().all(Value::is_u64)
        }),
    }
}

fn list<'a>(object: &'a Object, key: &str) -> Option<&'a [Value]> {
    match object.get(key) {
        None => Some(&[]),
        Some(value) => value.as_array().map(Vec::as_slice),
    }
}

fn objects<'a>(object: &'a Object, key: &str) -> Option<impl Iterator<Item = Option<&'a Object>>> {
    Some(list(object, key)?.iter().map(Value::as_object))
}

fn text<'a>(object: &'a Object, key: &str) -> Option<&'a str> {
    match object.get(key) {
        None => Some(""),
        Some(value) => value.as_str(),
    }
}

fn required_text(object: &Object, key: &str) -> Option<String> {
    object.get(key)?.as_str().map(str::to_owned)
}

fn count(object: &Object, key: &str) -> Option<usize> {
    match object.get(key) {
        None => Some(0),
        Some(value) => usize::try_from(value.as_u64()?).ok(),
    }
}

fn flag(object: &Object, key: &str, default: bool) -> Option<bool> {
    match object.get(key) {
        None => Some(default),
        Some(value) => value.as_bool(),
    }
}

#[cfg(test)]
mod tests;
