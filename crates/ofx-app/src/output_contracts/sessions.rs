use std::fmt::Write as _;

use ofx_cli::OutputFormat;
use ofx_session::SessionSummary;
use ofx_text::encode_terminal_safe;
use serde_json::{Map, Value, json};

use crate::context::civil_from_unix_days;

const FALLBACK_TITLE: &str = "Untitled session";
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;
const SCRIPT_LABELS: [(&str, &str); 8] = [
    ("Latn", "Latin script"),
    ("Hani", "Han script"),
    ("Arab", "Arabic script"),
    ("Hebr", "Hebrew script"),
    ("Cyrl", "Cyrillic script"),
    ("Grek", "Greek script"),
    ("Deva", "Devanagari script"),
    ("Thai", "Thai script"),
];
const LANGUAGE_LABELS: [(&str, &str); 15] = [
    ("en", "English"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("zh", "Chinese"),
    ("ar", "Arabic"),
    ("he", "Hebrew"),
    ("ru", "Russian"),
    ("el", "Greek"),
    ("hi", "Hindi"),
    ("th", "Thai"),
];

pub struct SessionListSnapshot<'a> {
    pub sessions: &'a [SessionSummary],
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub skipped_invalid: usize,
    pub all_workspaces: bool,
}

impl SessionListSnapshot<'_> {
    pub fn render(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Text => self.render_text(),
            OutputFormat::Json => {
                let mut line = Value::Object(self.json()).to_string();
                line.push('\n');
                line
            }
        }
    }

    fn render_text(&self) -> String {
        if self.sessions.is_empty() && self.skipped_invalid == 0 {
            return "[sessions] no saved sessions\n".to_owned();
        }
        let mut out = String::new();
        if self.sessions.is_empty() {
            out.push_str("[sessions] no readable saved sessions\n");
        } else {
            let _ = writeln!(out, "[sessions] {} saved", self.sessions.len());
            for session in self.sessions {
                let title = session.title.as_deref().unwrap_or(FALLBACK_TITLE);
                let _ = writeln!(out, " - {}", safe(title));
                let plural = if session.history_len == 1 { "" } else { "s" };
                let _ = write!(
                    out,
                    "   id={} | {} turn{plural}",
                    session.id, session.history_len
                );
                if let Some(label) = language_label(&session.conversation_language) {
                    let _ = write!(out, " | {}", safe(label));
                }
                let _ = write!(out, " | updated {}", utc_timestamp(session.updated_at_ms));
                if let Some(marker) = session.source.marker() {
                    let _ = write!(out, " | {marker}");
                }
                out.push('\n');
            }
        }
        if self.has_more {
            let _ = writeln!(
                out,
                "[sessions] more saved sessions; continue with `oh-fx sessions {}--cursor {}`",
                if self.all_workspaces { "--all " } else { "" },
                self.next_cursor.as_deref().unwrap_or_default()
            );
        }
        if self.skipped_invalid > 0 {
            let _ = writeln!(
                out,
                "[sessions] warning: skipped {} unreadable saved session{}; run `oh-fx doctor` for recovery guidance",
                self.skipped_invalid,
                if self.skipped_invalid == 1 { "" } else { "s" }
            );
        }
        out
    }

    fn json(&self) -> Map<String, Value> {
        let mut object = Map::new();
        object.insert("kind".to_owned(), json!("sessions"));
        object.insert("count".to_owned(), json!(self.sessions.len()));
        if self.sessions.is_empty() && self.skipped_invalid == 0 {
            object.insert("sessions".to_owned(), json!([]));
            return object;
        }
        if self.skipped_invalid > 0 {
            object.insert("skipped_invalid".to_owned(), json!(self.skipped_invalid));
        }
        if self.has_more {
            object.insert("has_more".to_owned(), json!(true));
            object.insert(
                "next_cursor".to_owned(),
                json!(self.next_cursor.as_deref().unwrap_or_default()),
            );
        }
        let sessions: Vec<Value> = self
            .sessions
            .iter()
            .map(|session| {
                let mut row = json!({
                    "id": session.id,
                    "title": session.title.as_deref().unwrap_or(FALLBACK_TITLE),
                    "preview": Value::Null,
                    "workspace_root": session.workspace_root,
                    "origin_workspace_root": session.origin_workspace_root,
                    "created_at_ms": session.created_at_ms,
                    "updated_at_ms": session.updated_at_ms,
                    "history_len": session.history_len,
                    "conversation_language": session.conversation_language,
                });
                if let (Some(marker), Some(fields)) = (session.source.marker(), row.as_object_mut())
                {
                    fields.insert("source".to_owned(), json!(marker));
                }
                row
            })
            .collect();
        object.insert("sessions".to_owned(), json!(sessions));
        object
    }
}

pub fn session_lookup_message(code: &str) -> Option<&'static str> {
    Some(match code {
        "NoSavedSessions" => "no saved sessions for this workspace",
        "NoReadableSessions" => {
            "saved sessions are unreadable; run `oh-fx doctor` for recovery guidance"
        }
        "SessionNotFound" => "record not found",
        "InvalidSessionFormat" => "record is corrupt; run `oh-fx doctor` for recovery guidance",
        "UnsupportedSessionSchema" => "record uses an unsupported session version",
        "InvalidSessionId" => "invalid session id",
        "SessionBusy" | "SessionLockUnsupported" => {
            "session is busy or the filesystem cannot provide the required lock"
        }
        "SessionPathUnsafe" | "DurablePathUnsafe" | "PrivateStatePermissionsUnsupported" => {
            "durable session storage is unsafe or does not support required private permissions"
        }
        "DurableLayoutFailed" | "SessionStoreUnavailable" => "durable session store is unavailable",
        "HomeNotSet" => "HOME is not set",
        "FxSessionOpen" => "fx has this session open; close it in fx, then resume it here",
        "FxCompactionUnfinished" => {
            "fx has not finished compacting this session; open it in fx once, then resume it here"
        }
        "FxSessionUnreadable" => {
            "this fx session holds data oh-fx cannot read yet; keep using it in fx"
        }
        _ => return None,
    })
}

fn language_label(tag: &str) -> Option<&str> {
    if tag.eq_ignore_ascii_case("und") {
        return None;
    }
    if tag.len() > 4
        && let Some(script) = tag
            .get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case("und-"))
            .map(|_| &tag[4..])
    {
        return Some(
            SCRIPT_LABELS
                .iter()
                .find(|(code, _)| script.eq_ignore_ascii_case(code))
                .map_or(tag, |(_, label)| label),
        );
    }
    let primary = tag.split('-').next().unwrap_or(tag);
    Some(
        LANGUAGE_LABELS
            .iter()
            .find(|(code, _)| primary.eq_ignore_ascii_case(code))
            .map_or(tag, |(_, label)| label),
    )
}

fn utc_timestamp(timestamp_ms: i64) -> String {
    if !(0..=MAX_TIMESTAMP_MS).contains(&timestamp_ms) {
        return "unknown".to_owned();
    }
    let seconds = timestamp_ms / 1000;
    let (year, month, day) = civil_from_unix_days(seconds / 86_400);
    let of_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03} UTC",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60,
        timestamp_ms % 1000
    )
}

fn safe(raw: &str) -> String {
    encode_terminal_safe(raw.as_bytes(), usize::MAX).text
}

#[cfg(test)]
mod tests;
