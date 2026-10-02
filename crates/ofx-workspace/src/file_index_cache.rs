use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_config::PrivateDir;
use ofx_text::lowercase_hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::file_index::{
    Candidate, CandidateKind, MAX_INDEXED_FILES, MAX_PATH_LEN, is_terminal_safe,
};

const MAGIC: &[u8] = b"fx-file-index-v1\n";
const DIRECTORY: &str = "file-index";
const MAX_BYTES: usize = 64 * 1024 * 1024;
const DIGEST_BYTES: usize = 32;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    written_at_ms: i64,
    roots: Vec<String>,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    kind: u8,
}

#[derive(Serialize)]
struct PayloadWire<'a> {
    written_at_ms: i64,
    roots: &'a [&'a str],
    entries: &'a [Entry],
}

pub(crate) fn load(cache_dir: &Path, roots: &[&Path]) -> Option<Vec<Candidate>> {
    let roots = utf8_roots(roots)?;
    let directory = PrivateDir::open_existing(&cache_dir.join(DIRECTORY)).ok()??;
    let bytes = directory
        .read_private(&file_name(&roots), MAX_BYTES)
        .ok()??;
    let payload = bytes.strip_prefix(MAGIC)?;
    if payload.len() < DIGEST_BYTES {
        return None;
    }
    let (digest, payload) = payload.split_at(DIGEST_BYTES);
    if Sha256::digest(payload).as_slice() != digest {
        return None;
    }
    let parsed: Payload = serde_json::from_slice(payload).ok()?;
    if parsed.written_at_ms < 0
        || parsed.roots.is_empty()
        || parsed.entries.len() > MAX_INDEXED_FILES
        || parsed.roots != roots
    {
        return None;
    }
    parsed
        .entries
        .into_iter()
        .map(|entry| {
            let kind = match entry.kind {
                0 => CandidateKind::File,
                1 => CandidateKind::Directory,
                _ => return None,
            };
            (!entry.path.is_empty()
                && entry.path.len() <= MAX_PATH_LEN
                && is_terminal_safe(&entry.path))
            .then_some(Candidate {
                path: entry.path,
                kind,
            })
        })
        .collect()
}

pub(crate) fn save(cache_dir: &Path, roots: &[&Path], candidates: &[Candidate]) -> Option<()> {
    let roots = utf8_roots(roots)?;
    if roots.is_empty() {
        return None;
    }
    let name = file_name(&roots);
    if candidates.is_empty() {
        let directory = PrivateDir::open_existing(&cache_dir.join(DIRECTORY)).ok()??;
        directory.remove(&name).ok()?;
        return Some(());
    }
    let entries: Vec<Entry> = candidates
        .iter()
        .filter(|candidate| {
            !candidate.path.is_empty()
                && candidate.path.len() <= MAX_PATH_LEN
                && is_terminal_safe(&candidate.path)
        })
        .map(|candidate| Entry {
            path: candidate.path.clone(),
            kind: match candidate.kind {
                CandidateKind::File => 0,
                CandidateKind::Directory => 1,
            },
        })
        .collect();
    let payload = serde_json::to_vec(&PayloadWire {
        written_at_ms: now_ms(),
        roots: &roots,
        entries: &entries,
    })
    .ok()?;
    if payload.len() > MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(MAGIC.len() + DIGEST_BYTES + payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&Sha256::digest(&payload));
    bytes.extend_from_slice(&payload);
    let directory = PrivateDir::open_or_create(&cache_dir.join(DIRECTORY)).ok()?;
    directory.replace(&name, &bytes).ok()
}

fn utf8_roots<'a>(roots: &[&'a Path]) -> Option<Vec<&'a str>> {
    roots.iter().map(|root| root.to_str()).collect()
}

fn file_name(roots: &[&str]) -> String {
    let mut digest = Sha256::new();
    for root in roots {
        digest.update(root.as_bytes());
        digest.update([0]);
    }
    format!("{}.idx", lowercase_hex(&digest.finalize()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    fn candidates() -> Vec<Candidate> {
        vec![
            Candidate {
                path: "src/main.rs".to_owned(),
                kind: CandidateKind::File,
            },
            Candidate {
                path: "docs".to_owned(),
                kind: CandidateKind::Directory,
            },
        ]
    }

    #[test]
    fn the_cache_round_trips_its_scope_and_rejects_tampering() {
        let cache = tempfile::tempdir().unwrap();
        let roots = [Path::new("/workspace")];
        assert!(load(cache.path(), &roots).is_none());
        save(cache.path(), &roots, &candidates()).unwrap();
        assert_eq!(load(cache.path(), &roots), Some(candidates()));
        assert!(load(cache.path(), &[Path::new("/elsewhere")]).is_none());

        let path = cache
            .path()
            .join(DIRECTORY)
            .join(file_name(&["/workspace"]));
        let saved = fs::read(&path).unwrap();
        assert!(saved.starts_with(MAGIC));
        let payload = &saved[MAGIC.len() + DIGEST_BYTES..];
        let text = std::str::from_utf8(payload).unwrap();
        assert!(text.starts_with("{\"written_at_ms\":"), "{text}");
        assert!(text.ends_with(",\"roots\":[\"/workspace\"],\"entries\":[{\"path\":\"src/main.rs\",\"kind\":0},{\"path\":\"docs\",\"kind\":1}]}"), "{text}");

        let mut flipped = saved.clone();
        let last = flipped.len() - 4;
        flipped[last] ^= 1;
        fs::write(&path, &flipped).unwrap();
        assert!(load(cache.path(), &roots).is_none());
    }

    #[test]
    fn unsafe_entries_never_reach_disk_and_empty_scans_drop_the_cache() {
        let cache = tempfile::tempdir().unwrap();
        let roots = [Path::new("/workspace")];
        let unsafe_only = [Candidate {
            path: "bad\u{1b}path".to_owned(),
            kind: CandidateKind::File,
        }];
        save(cache.path(), &roots, &unsafe_only).unwrap();
        assert_eq!(load(cache.path(), &roots), Some(Vec::new()));
        save(cache.path(), &roots, &[]).unwrap();
        assert!(load(cache.path(), &roots).is_none());
        let names: Vec<_> = fs::read_dir(cache.path().join(DIRECTORY))
            .unwrap()
            .collect();
        assert!(names.is_empty());
    }

    #[test]
    fn shared_cache_files_are_ignored() {
        use std::os::unix::fs::PermissionsExt;

        let cache = tempfile::tempdir().unwrap();
        let roots = [Path::new("/workspace")];
        save(cache.path(), &roots, &candidates()).unwrap();
        let path = cache
            .path()
            .join(DIRECTORY)
            .join(file_name(&["/workspace"]));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(cache.path(), &roots).is_none());
    }

    #[test]
    fn roots_hash_with_a_separator_after_each_root() {
        let name = file_name(&["/workspace"]);
        let expected = lowercase_hex(&Sha256::digest(b"/workspace\0"));
        assert_eq!(name, format!("{expected}.idx"));
        assert_eq!(Path::new(&name).as_os_str().as_bytes().len(), 68);
    }
}
