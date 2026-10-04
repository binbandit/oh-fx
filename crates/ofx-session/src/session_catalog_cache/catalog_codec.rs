use sha2::{Digest, Sha256};

use super::fingerprint::Fingerprint;
use super::{KNOWN_FLAGS, MAX_BYTES, MAX_RECORDS, Row, RowSummary};
use crate::session_layout::is_valid_session_id;

pub(super) const MAGIC: &[u8] = b"fx-resume-catalog-v6\n";
const DIGEST_BYTES: usize = 32;
const ABSENT: u32 = u32::MAX;
const EXCLUDED: u8 = 0;
const VISIBLE: u8 = 1;

pub(super) fn encode_catalog(rows: &[Row]) -> Option<Vec<u8>> {
    if rows.len() > MAX_RECORDS {
        return None;
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(&u32::try_from(rows.len()).ok()?.to_le_bytes());
    for row in rows {
        write_row(&mut payload, row)?;
        if payload.len() > MAX_BYTES - MAGIC.len() - DIGEST_BYTES {
            return None;
        }
    }
    let mut bytes = Vec::with_capacity(MAGIC.len() + DIGEST_BYTES + payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&Sha256::digest(&payload));
    bytes.extend_from_slice(&payload);
    Some(bytes)
}

pub(super) fn decode_catalog(bytes: &[u8]) -> Option<Vec<Row>> {
    let rest = bytes.strip_prefix(MAGIC)?;
    if rest.len() < DIGEST_BYTES + size_of::<u32>() {
        return None;
    }
    let (digest, payload) = rest.split_at(DIGEST_BYTES);
    if Sha256::digest(payload).as_slice() != digest {
        return None;
    }
    let mut cursor = Cursor { bytes: payload };
    let count = usize::try_from(cursor.u32()?).ok()?;
    if count > MAX_RECORDS {
        return None;
    }
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        rows.push(read_row(&mut cursor)?);
    }
    cursor.bytes.is_empty().then_some(rows)
}

fn write_row(payload: &mut Vec<u8>, row: &Row) -> Option<()> {
    write_string(payload, &row.id)?;
    payload.extend_from_slice(&row.fingerprint);
    let Some(summary) = &row.summary else {
        payload.push(EXCLUDED);
        return Some(());
    };
    payload.push(VISIBLE);
    payload.push(summary.flags);
    payload.extend_from_slice(&summary.created_at_ms.to_le_bytes());
    payload.extend_from_slice(&summary.updated_at_ms.to_le_bytes());
    payload.extend_from_slice(&summary.history_len.to_le_bytes());
    for value in [
        &summary.workspace_root,
        &summary.origin_workspace_root,
        &summary.title,
        &summary.preview,
    ] {
        match value {
            Some(text) => write_string(payload, text)?,
            None => payload.extend_from_slice(&ABSENT.to_le_bytes()),
        }
    }
    write_string(payload, &summary.language)
}

fn write_string(payload: &mut Vec<u8>, text: &str) -> Option<()> {
    payload.extend_from_slice(&u32::try_from(text.len()).ok()?.to_le_bytes());
    payload.extend_from_slice(text.as_bytes());
    Some(())
}

fn read_row(cursor: &mut Cursor<'_>) -> Option<Row> {
    let id = cursor.string()?;
    if !is_valid_session_id(&id) {
        return None;
    }
    let fingerprint: Fingerprint = cursor.take(DIGEST_BYTES)?.try_into().ok()?;
    let summary = match cursor.byte()? {
        EXCLUDED => None,
        VISIBLE => {
            let flags = cursor.byte()?;
            if flags & !KNOWN_FLAGS != 0 {
                return None;
            }
            let summary = RowSummary {
                flags,
                created_at_ms: i64::from_le_bytes(cursor.array()?),
                updated_at_ms: i64::from_le_bytes(cursor.array()?),
                history_len: u64::from_le_bytes(cursor.array()?),
                workspace_root: cursor.optional_string().ok()?,
                origin_workspace_root: cursor.optional_string().ok()?,
                title: cursor.optional_string().ok()?,
                preview: cursor.optional_string().ok()?,
                language: cursor.string()?,
            };
            if !summary.persistable() {
                return None;
            }
            Some(summary)
        }
        _ => return None,
    };
    Some(Row {
        id,
        fingerprint,
        summary,
    })
}

struct Malformed;

struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if count > self.bytes.len() {
            return None;
        }
        let (taken, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Some(taken)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn byte(&mut self) -> Option<u8> {
        Some(self.array::<1>()?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.array()?))
    }

    fn string(&mut self) -> Option<String> {
        let length = usize::try_from(self.u32()?).ok()?;
        String::from_utf8(self.take(length)?.to_vec()).ok()
    }

    fn optional_string(&mut self) -> Result<Option<String>, Malformed> {
        let length = self.u32().ok_or(Malformed)?;
        if length == ABSENT {
            return Ok(None);
        }
        let length = usize::try_from(length).map_err(|_| Malformed)?;
        let bytes = self.take(length).ok_or(Malformed)?;
        String::from_utf8(bytes.to_vec())
            .map(Some)
            .map_err(|_| Malformed)
    }
}
