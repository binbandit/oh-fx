use std::fmt::Write as _;

use ofx_agent::checkpoint_model_text;
use ofx_cli::OutputFormat;
use ofx_session::{ArchivedTurn, SessionArchive, TurnExecution};
use serde_json::{Value, json};

pub struct SessionDetailSnapshot<'a> {
    pub archive: &'a SessionArchive,
}

impl SessionDetailSnapshot<'_> {
    pub fn render(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Text => self.render_text(),
            OutputFormat::Json => {
                let mut line = self.json().to_string();
                line.push('\n');
                line
            }
        }
    }

    fn render_text(&self) -> String {
        let archive = self.archive;
        let mut out = format!(
            "[session] {}\ncreated_at_ms: {}\nupdated_at_ms: {}\nlanguage: {}\nhistory_len: {}\n",
            archive.id,
            archive.created_at_ms,
            archive.updated_at_ms,
            archive.conversation_language,
            archive.turns.len()
        );
        if let Some(marker) = archive.source.marker() {
            let _ = writeln!(out, "source: {marker}");
        }
        if archive.turns.is_empty() {
            out.push_str("\n(no history yet)\n");
            return out;
        }
        for (index, turn) in archive.turns.iter().enumerate() {
            let _ = write!(out, "\n[turn {}]\n", index + 1);
            write_turn_text(&mut out, turn);
        }
        out
    }

    fn json(&self) -> Value {
        let archive = self.archive;
        let mut object = json!({
            "kind": "session_detail",
            "id": archive.id,
            "created_at_ms": archive.created_at_ms,
            "updated_at_ms": archive.updated_at_ms,
            "history_len": archive.turns.len(),
            "conversation_language": archive.conversation_language,
            "history": archive.turns.iter().map(turn_json).collect::<Vec<_>>(),
        });
        if let (Some(marker), Some(fields)) = (archive.source.marker(), object.as_object_mut()) {
            fields.insert("source".to_owned(), json!(marker));
        }
        object
    }
}

fn write_turn_text(out: &mut String, turn: &ArchivedTurn) {
    match turn {
        ArchivedTurn::Compacted(compacted) => {
            let _ = writeln!(
                out,
                "[compacted] removed_turns={} compactions={}",
                compacted.removed_turn_count, compacted.compaction_count
            );
            let rendered = checkpoint_model_text(&compacted.summary);
            write_text_block(out, rendered.as_deref().unwrap_or(&compacted.summary));
        }
        ArchivedTurn::Replied {
            user,
            assistant,
            execution,
        } => {
            write_user_text(out, user);
            write_execution_text(out, execution);
            out.push_str("[assistant]\n");
            write_text_block(out, assistant);
        }
        ArchivedTurn::Interrupted {
            user,
            assistant,
            tool_call,
            execution,
        } => {
            write_user_text(out, user);
            write_execution_text(out, execution);
            if let Some(assistant) = assistant {
                out.push_str("[assistant]\n");
                write_text_block(out, assistant);
            }
            out.push_str("[interrupted]\n");
            match tool_call {
                Some(call) => {
                    let _ = writeln!(out, "tool_call_id: {}", call.call_id);
                    let _ = writeln!(out, "tool_name: {}", call.tool_name);
                }
                None => out.push_str("tool: (none)\n"),
            }
        }
    }
}

fn write_user_text(out: &mut String, user: &str) {
    out.push_str("[user]\n");
    write_text_block(out, user);
}

fn write_execution_text(out: &mut String, execution: &TurnExecution) {
    if execution.is_empty() {
        return;
    }
    out.push_str("[execution]\n");
    for step in &execution.steps {
        if let Some(assistant) = &step.assistant {
            out.push_str("assistant:\n");
            write_text_block(out, assistant);
        }
        for call in &step.calls {
            let _ = writeln!(out, "tool_call: {} {}", call.call_id, call.tool_name);
            out.push_str("arguments:\n");
            write_text_block(out, &call.arguments_json);
        }
        for result in &step.results {
            let _ = writeln!(
                out,
                "tool_result: {} {} {}",
                result.call_id,
                result.tool_name,
                result.status.label()
            );
            out.push_str("output:\n");
            write_text_block(out, result.output());
        }
    }
    for file in &execution.files {
        let _ = writeln!(
            out,
            "file: {} {} {}",
            file.action.label(),
            file.status.label(),
            file.path
        );
    }
}

fn write_text_block(out: &mut String, text: &str) {
    if text.is_empty() {
        out.push_str("(empty)\n");
        return;
    }
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
}

fn turn_json(turn: &ArchivedTurn) -> Value {
    match turn {
        ArchivedTurn::Compacted(compacted) => json!({
            "kind": "compacted_summary",
            "summary": compacted.summary,
            "removed_turn_count": compacted.removed_turn_count,
            "compaction_count": compacted.compaction_count,
        }),
        ArchivedTurn::Replied {
            user,
            assistant,
            execution,
        } => json!({
            "kind": "assistant",
            "user": user_json(user),
            "assistant": assistant,
            "execution": execution.presentation_json(),
        }),
        ArchivedTurn::Interrupted {
            user,
            assistant,
            tool_call,
            execution,
        } => {
            let mut object = json!({
                "kind": "interrupted",
                "user": user_json(user),
                "assistant": assistant,
                "tool_call": tool_call.as_ref().map(|call| json!({
                    "id": call.call_id,
                    "name": call.tool_name,
                    "arguments_json": call.arguments_json,
                })),
                "completed_tool_names": [],
            });
            if let (false, Some(fields)) = (execution.is_empty(), object.as_object_mut()) {
                fields.insert("execution".to_owned(), execution.presentation_json());
            }
            object
        }
    }
}

fn user_json(text: &str) -> Value {
    json!({
        "text": text,
        "images": [],
    })
}

#[cfg(test)]
mod tests;
