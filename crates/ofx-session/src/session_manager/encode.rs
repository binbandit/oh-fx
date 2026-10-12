use super::fields::Tag;
use super::frame::{TooLarge, seal_frame};
use super::schema::{Body, Host, Kind, Reason, Role, SetKey};

const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) id: String,
    pub(crate) workspace: String,
    pub(crate) role: Role,
    pub(crate) host: Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Line<'a> {
    TurnStarted(u64),
    Item {
        turn: u64,
        item_type: &'a str,
        data: &'a [u8],
        blobs: &'a [String],
    },
    TurnCommitted(u64),
    TurnInterrupted(u64, Reason),
    Set(SetKey, &'a [u8]),
    Closed,
}

impl Line<'_> {
    fn kind(&self) -> Kind {
        match self {
            Self::TurnStarted(_) => Kind::TurnStarted,
            Self::Item { .. } => Kind::Item,
            Self::TurnCommitted(_) => Kind::TurnCommitted,
            Self::TurnInterrupted(..) => Kind::TurnInterrupted,
            Self::Set(..) => Kind::Set,
            Self::Closed => Kind::Closed,
        }
    }

    pub(crate) fn body(&self) -> Body<'_> {
        match *self {
            Self::TurnStarted(turn) => Body::TurnStarted { turn },
            Self::Item {
                item_type,
                data,
                blobs,
                ..
            } => Body::Item {
                item_type: item_type.to_owned(),
                data,
                blobs: blobs.to_vec(),
            },
            Self::TurnCommitted(_) => Body::TurnCommitted,
            Self::TurnInterrupted(turn, reason) => Body::TurnInterrupted { turn, reason },
            Self::Set(key, value) => Body::Set {
                key,
                value,
                blobs: Vec::new(),
            },
            Self::Closed => Body::Closed,
        }
    }

    pub(crate) fn names_blobs(&self) -> &[String] {
        match self {
            Self::Item { blobs, .. } => blobs,
            _ => &[],
        }
    }
}

pub(crate) fn append_created(
    out: &mut Vec<u8>,
    ts_ms: u64,
    identity: &Identity,
) -> Result<(), TooLarge> {
    let start = out.len();
    append_header(out, 1, ts_ms, Kind::SessionCreated);
    string_field(out, "id", &identity.id);
    string_field(out, "workspace", &identity.workspace);
    string_field(out, "role", identity.role.name());
    string_field(out, "host", identity.host.name());
    seal_frame(out, start)
}

pub(crate) fn append_line(
    out: &mut Vec<u8>,
    seq: u64,
    ts_ms: u64,
    line: &Line<'_>,
) -> Result<(), TooLarge> {
    let start = out.len();
    append_header(out, seq, ts_ms, line.kind());
    match *line {
        Line::TurnStarted(turn) | Line::TurnCommitted(turn) => number_field(out, "turn", turn),
        Line::Item {
            turn,
            item_type,
            data,
            blobs,
        } => {
            number_field(out, "turn", turn);
            string_field(out, "type", item_type);
            raw_field(out, "data", data);
            if !blobs.is_empty() {
                key(out, "blobs");
                out.push(b'[');
                for (at, hash) in blobs.iter().enumerate() {
                    if at > 0 {
                        out.push(b',');
                    }
                    append_json_string(out, hash);
                }
                out.push(b']');
            }
        }
        Line::TurnInterrupted(turn, reason) => {
            number_field(out, "turn", turn);
            string_field(out, "reason", reason.name());
        }
        Line::Set(setting, value) => {
            string_field(out, "key", setting.name());
            raw_field(out, "value", value);
        }
        Line::Closed => {}
    }
    seal_frame(out, start)
}

fn append_header(out: &mut Vec<u8>, seq: u64, ts_ms: u64, kind: Kind) {
    out.extend_from_slice(
        format!(
            "{{\"v\":{VERSION},\"seq\":{seq},\"ts\":{ts_ms},\"kind\":\"{}\"",
            kind.name()
        )
        .as_bytes(),
    );
}

fn key(out: &mut Vec<u8>, name: &str) {
    out.push(b',');
    append_json_string(out, name);
    out.push(b':');
}

fn string_field(out: &mut Vec<u8>, name: &str, value: &str) {
    key(out, name);
    append_json_string(out, value);
}

fn number_field(out: &mut Vec<u8>, name: &str, value: u64) {
    key(out, name);
    out.extend_from_slice(value.to_string().as_bytes());
}

fn raw_field(out: &mut Vec<u8>, name: &str, value: &[u8]) {
    key(out, name);
    out.extend_from_slice(value);
}

pub(crate) fn append_json_string(out: &mut Vec<u8>, text: &str) {
    let bytes = text.as_bytes();
    out.push(b'"');
    let mut start = 0;
    for (at, &byte) in bytes.iter().enumerate() {
        let escape: &[u8] = match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x20.. => continue,
            _ => b"",
        };
        out.extend_from_slice(&bytes[start..at]);
        if escape.is_empty() {
            out.extend_from_slice(format!("\\u{byte:04x}").as_bytes());
        } else {
            out.extend_from_slice(escape);
        }
        start = at + 1;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

#[cfg(test)]
mod tests;
