use super::fields::{BadBody, Fields, Tag};

const MAX_KIND_NAME_BYTES: usize = 32;
const MAX_ID_BYTES: usize = 255;
const BLOB_HASH_BYTES: usize = 64;
const RESERVED_NAMES: [&str; 2] = ["index.jsonl", "index.lock"];

macro_rules! tags {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum $name {
            $($variant),+
        }

        impl Tag for $name {
            const ALL: &'static [Self] = &[$(Self::$variant),+];

            fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }
    };
}

tags!(Kind {
    SessionCreated => "session_created",
    TurnStarted => "turn_started",
    Item => "item",
    Compacted => "compacted",
    TurnCommitted => "turn_committed",
    TurnInterrupted => "turn_interrupted",
    Set => "set",
    ChildSpawned => "child_spawned",
    ChildFinished => "child_finished",
    Snapshot => "snapshot",
    Closed => "closed",
});

tags!(Role {
    Root => "root",
    Child => "child",
});

tags!(Host {
    App => "app",
    Ask => "ask",
    Acp => "acp",
    Sdk => "sdk",
    Child => "child",
});

tags!(Reason {
    Cancel => "cancel",
    Failed => "failed",
    Closed => "closed",
    Crash => "crash",
});

tags!(Outcome {
    Ok => "ok",
    Failed => "failed",
    Cancelled => "cancelled",
    Interrupted => "interrupted",
    Lost => "lost",
});

tags!(SetKey {
    Prefs => "prefs",
    Title => "title",
    Permissions => "permissions",
    Usage => "usage",
    Workspace => "workspace",
    Language => "language",
    ClientPrompt => "client_prompt",
    ToolIdentities => "tool_identities",
    MovedFiles => "moved_files",
    CompactionRecords => "compaction_records",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) seq: u64,
    pub(crate) ts_ms: u64,
    pub(crate) kind: Option<Kind>,
}

pub(crate) fn parse_header(line: &[u8]) -> Option<Header> {
    let mut cursor = Cursor { bytes: line, at: 0 };
    cursor.literal(b"{\"v\":")?;
    let version = cursor.number(u64::from(u32::MAX))?;
    cursor.literal(b",\"seq\":")?;
    let seq = cursor.number(u64::MAX)?;
    cursor.literal(b",\"ts\":")?;
    let ts_ms = cursor.number(u64::MAX)?;
    cursor.literal(b",\"kind\":\"")?;
    let name_start = cursor.at;
    while line
        .get(cursor.at)
        .is_some_and(|&byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        cursor.at += 1;
    }
    let name = &line[name_start..cursor.at];
    cursor.literal(b"\"")?;
    if version == 0 || seq == 0 || name.is_empty() || name.len() > MAX_KIND_NAME_BYTES {
        return None;
    }
    let name = std::str::from_utf8(name).ok()?;
    let kind = Kind::ALL.iter().copied().find(|kind| kind.name() == name);
    line.get(cursor.at)
        .is_none_or(|&byte| byte == b',')
        .then_some(Header { seq, ts_ms, kind })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn literal(&mut self, expected: &[u8]) -> Option<()> {
        self.bytes[self.at..].starts_with(expected).then(|| {
            self.at += expected.len();
        })
    }

    fn number(&mut self, max: u64) -> Option<u64> {
        let start = self.at;
        let mut value: u64 = 0;
        while let Some(&digit) = self.bytes.get(self.at).filter(|byte| byte.is_ascii_digit()) {
            value = value
                .checked_mul(10)?
                .checked_add(u64::from(digit - b'0'))?;
            self.at += 1;
        }
        let digits = self.at - start;
        if digits == 0 || (digits > 1 && self.bytes[start] == b'0') || value > max {
            return None;
        }
        Some(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Body<'a> {
    SessionCreated {
        role: Role,
        fork_seq: u64,
    },
    TurnStarted {
        turn: u64,
    },
    Item {
        item_type: String,
        data: &'a [u8],
        blobs: Vec<String>,
    },
    Compacted {
        data: &'a [u8],
    },
    TurnCommitted,
    TurnInterrupted {
        turn: u64,
        reason: Reason,
    },
    Set {
        key: SetKey,
        value: &'a [u8],
        blobs: Vec<String>,
    },
    ChildSpawned {
        child: String,
        work_id: String,
        data: Option<&'a [u8]>,
    },
    ChildFinished {
        child: String,
        work_id: String,
        outcome: Outcome,
        data: Option<&'a [u8]>,
    },
    Snapshot {
        state: &'a [u8],
    },
    Closed,
}

impl Body<'_> {
    pub(crate) fn blob_refs(&self) -> &[String] {
        match self {
            Self::Item { blobs, .. } | Self::Set { blobs, .. } => blobs,
            _ => &[],
        }
    }
}

pub(crate) fn parse_line_body(kind: Kind, line: &[u8]) -> Result<Body<'_>, BadBody> {
    let object = line.strip_suffix(b"\n").ok_or(BadBody)?;
    let fields = Fields::parse_object(object).ok_or(BadBody)?;
    Ok(match kind {
        Kind::SessionCreated => {
            fields.req_str("id")?;
            fields.req_str("workspace")?;
            let role = fields.req_tag("role")?;
            fields.req_tag::<Host>("host")?;
            fields.opt_str("parent")?;
            let fork_seq = match fields.opt_struct("forked_from", &["id", "seq"])? {
                Some(origin) => {
                    Fields::string_value(origin[0])?;
                    Fields::u64_value(origin[1])?
                }
                None => 0,
            };
            Body::SessionCreated { role, fork_seq }
        }
        Kind::TurnStarted => Body::TurnStarted {
            turn: fields.req_u64("turn")?,
        },
        Kind::TurnCommitted => {
            fields.req_u64("turn")?;
            Body::TurnCommitted
        }
        Kind::Item => {
            fields.req_u64("turn")?;
            Body::Item {
                item_type: fields.req_str("type")?.into_owned(),
                data: fields.raw("data").ok_or(BadBody)?,
                blobs: fields.opt_str_list("blobs")?.unwrap_or_default(),
            }
        }
        Kind::Compacted => {
            fields.opt_u64("turn")?;
            Body::Compacted {
                data: fields.raw("data").ok_or(BadBody)?,
            }
        }
        Kind::TurnInterrupted => Body::TurnInterrupted {
            turn: fields.req_u64("turn")?,
            reason: fields.req_tag("reason")?,
        },
        Kind::Set => Body::Set {
            key: fields.req_tag("key")?,
            value: fields.raw("value").ok_or(BadBody)?,
            blobs: fields.opt_str_list("blobs")?.unwrap_or_default(),
        },
        Kind::ChildSpawned => Body::ChildSpawned {
            child: fields.req_str("child")?.into_owned(),
            work_id: fields.req_str("work_id")?.into_owned(),
            data: fields.raw("data"),
        },
        Kind::ChildFinished => Body::ChildFinished {
            child: fields.req_str("child")?.into_owned(),
            work_id: fields.req_str("work_id")?.into_owned(),
            outcome: fields.req_tag("outcome")?,
            data: fields.raw("data"),
        },
        Kind::Snapshot => {
            fields.req_u64("covers_seq")?;
            fields.opt_u64("compaction_offset")?;
            Body::Snapshot {
                state: fields.raw("state").ok_or(BadBody)?,
            }
        }
        Kind::Closed => Body::Closed,
    })
}

pub(crate) fn valid_id(id: &str) -> bool {
    (1..=MAX_ID_BYTES).contains(&id.len())
        && !id.starts_with('.')
        && !RESERVED_NAMES.contains(&id)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(crate) fn valid_blob_hash(hash: &str) -> bool {
    hash.len() == BLOB_HASH_BYTES
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests;
