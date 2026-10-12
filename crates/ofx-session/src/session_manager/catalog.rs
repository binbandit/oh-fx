use std::collections::HashMap;

use ofx_config::PrivateDir;

use super::fields::{BadBody, Fields, Tag};
use super::frame::check_frame;
use super::schema::{Host, Role};
use super::storage::{self, IoFault, StorageError};

const INDEX_NAME: &str = "index.jsonl";
const HOST_COUNT: usize = Host::ALL.len();

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) id: String,
    pub(crate) role: Role,
    pub(crate) workspace: String,
    pub(crate) title: Option<Vec<u8>>,
    pub(crate) language: Option<Vec<u8>>,
    pub(crate) created_ms: u64,
    pub(crate) updated_ms: u64,
    pub(crate) turns: u64,
    opened_ms: [u64; HOST_COUNT],
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Record {
    Put(Summary),
    Opened { id: String, host: Host, ts_ms: u64 },
    Del { id: String, ts_ms: u64 },
}

fn decode_record(line: &[u8]) -> Option<Record> {
    let payload = check_frame(line)?;
    let mut object = Vec::with_capacity(payload.len() + 1);
    object.extend_from_slice(payload);
    object.push(b'}');
    decode_object(&object).ok()
}

fn decode_object(object: &[u8]) -> Result<Record, BadBody> {
    let fields = Fields::parse_object(object).ok_or(BadBody)?;
    let op = fields.req_str("op")?;
    let id = fields.req_str("id")?.into_owned();
    match &*op {
        "put" => {
            let role = fields.req_tag::<Role>("role")?;
            fields.req_tag::<Host>("host")?;
            let workspace = fields.req_str("workspace")?.into_owned();
            let title = fields.raw("title").map(<[u8]>::to_vec);
            let language = fields.raw("language").map(<[u8]>::to_vec);
            fields.opt_str("parent")?;
            let created_ms = fields.req_u64("created")?;
            let updated_ms = fields.req_u64("updated")?;
            let turns = fields.req_u64("turns")?;
            let opened_ms = fields
                .req_u64_list("opened")?
                .try_into()
                .map_err(|_| BadBody)?;
            Ok(Record::Put(Summary {
                id,
                role,
                workspace,
                title,
                language,
                created_ms,
                updated_ms,
                turns,
                opened_ms,
            }))
        }
        "opened" => Ok(Record::Opened {
            id,
            host: fields.req_tag("host")?,
            ts_ms: fields.req_u64("ts")?,
        }),
        "del" => Ok(Record::Del {
            id,
            ts_ms: fields.req_u64("ts")?,
        }),
        _ => Err(BadBody),
    }
}

struct Entry {
    summary: Summary,
    deleted: bool,
}

#[derive(Default)]
struct Index {
    entries: Vec<Entry>,
    positions: HashMap<String, usize>,
}

impl Index {
    fn fold(bytes: &[u8]) -> Self {
        let mut index = Self::default();
        let mut start = 0;
        while let Some(newline) = bytes[start..].iter().position(|&byte| byte == b'\n') {
            let end = start + newline + 1;
            if let Some(record) = decode_record(&bytes[start..end]) {
                index.apply(record);
            }
            start = end;
        }
        index
    }

    fn apply(&mut self, record: Record) {
        match record {
            Record::Put(summary) => match self.positions.get(&summary.id) {
                Some(&position) => {
                    let entry = &mut self.entries[position];
                    if entry.deleted {
                        return;
                    }
                    let mut merged = summary;
                    for (opened, old) in merged.opened_ms.iter_mut().zip(entry.summary.opened_ms) {
                        *opened = (*opened).max(old);
                    }
                    entry.summary = merged;
                }
                None => self.insert(Entry {
                    summary,
                    deleted: false,
                }),
            },
            Record::Opened { id, host, ts_ms } => {
                if let Some(&position) = self.positions.get(&id) {
                    let slot = host as usize;
                    let opened = &mut self.entries[position].summary.opened_ms[slot];
                    *opened = (*opened).max(ts_ms);
                }
            }
            Record::Del { id, ts_ms } => match self.positions.get(&id) {
                Some(&position) => self.entries[position].deleted = true,
                None => self.insert(Entry {
                    summary: Summary {
                        id,
                        role: Role::Root,
                        workspace: String::new(),
                        title: None,
                        language: None,
                        created_ms: ts_ms,
                        updated_ms: ts_ms,
                        turns: 0,
                        opened_ms: [0; HOST_COUNT],
                    },
                    deleted: true,
                }),
            },
        }
    }

    fn insert(&mut self, entry: Entry) {
        self.positions
            .insert(entry.summary.id.clone(), self.entries.len());
        self.entries.push(entry);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListCursor {
    updated_ms: u64,
    id: String,
}

#[derive(Debug, Default)]
pub(crate) struct Page {
    pub(crate) items: Vec<Summary>,
    pub(crate) next: Option<ListCursor>,
}

pub(crate) fn list(
    root: &PrivateDir,
    cursor: Option<&ListCursor>,
    limit: usize,
) -> Result<Page, IoFault> {
    let index = Index::fold(&read_index(root)?);
    let candidates = sorted(index);
    let mut start = 0;
    if let Some(cursor) = cursor {
        while start < candidates.len() && !is_after(&candidates[start], cursor) {
            start += 1;
        }
    }
    let mut page = Page::default();
    let mut next = start;
    while next < candidates.len() && page.items.len() < limit {
        let candidate = &candidates[next];
        next += 1;
        if storage::is_directory(root, &candidate.id)? {
            page.items.push(candidate.clone());
        }
    }
    if next < candidates.len() && page.items.len() == limit {
        page.next = page.items.last().map(|last| ListCursor {
            updated_ms: last.updated_ms,
            id: last.id.clone(),
        });
    }
    Ok(page)
}

fn read_index(root: &PrivateDir) -> Result<Vec<u8>, IoFault> {
    match storage::read_file(root, INDEX_NAME) {
        Ok(bytes) => Ok(bytes),
        Err(StorageError::NotFound) => Ok(Vec::new()),
        Err(StorageError::Fault(fault)) => Err(fault),
    }
}

fn sorted(index: Index) -> Vec<Summary> {
    let mut out: Vec<Summary> = index
        .entries
        .into_iter()
        .filter(|entry| !entry.deleted && entry.summary.role == Role::Root)
        .map(|entry| entry.summary)
        .collect();
    out.sort_by(|a, b| {
        b.updated_ms
            .cmp(&a.updated_ms)
            .then_with(|| a.id.as_bytes().cmp(b.id.as_bytes()))
    });
    out
}

fn is_after(summary: &Summary, cursor: &ListCursor) -> bool {
    if summary.updated_ms != cursor.updated_ms {
        return summary.updated_ms < cursor.updated_ms;
    }
    cursor.id.as_bytes() < summary.id.as_bytes()
}

#[cfg(test)]
mod tests;
